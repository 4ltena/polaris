//! Production composition, owned until both IPC channels and engine have drained.
//! No environment-selected catalogs, executable or permission grants. The native
//! owner may pass only the optional Jev credential selected in setup.
use crate::*;
use polaris_core::{
    audit::AuditLog,
    constitution::AgentsRefresh,
    desktop_store::{
        BootstrapProvider, BootstrapTier, DesktopRoot, ExpectedBootstrapStore, SourceApplyIdentity,
        read_owner_bootstrap,
    },
    isolated_workspace::{self, Limits, Snapshot},
    local_execution::LocalRouter,
    prompt::assemble_always_on,
};
use polaris_provider::local::{Endpoint, LocalAdapter, Runtime};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("本番サービスの認証・設定・実行資源を確認できませんでした")]
    Generic,
    #[error("{0}")]
    Memory(crate::MemoryResourceError),
}

impl StartupError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Generic => 1,
            Self::Memory(error) => error.exit_code(),
        }
    }
}

/// Catalog schemas point into these sanitized copies. Retain them for every
/// child and cleanup, not merely until ConfiguredRunFactory construction.
pub struct StartedOwner {
    pub service: DesktopService,
    _catalogs: Vec<Snapshot>,
}

struct Clock {
    started: std::time::Instant,
    unix_ms: u64,
}
impl Clock {
    fn new() -> Result<Self, StartupError> {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StartupError::Generic)?
            .as_millis()
            .try_into()
            .map_err(|_| StartupError::Generic)?;
        Ok(Self {
            started: std::time::Instant::now(),
            unix_ms,
        })
    }
}
impl SourceApplyClock for Clock {
    fn now_ms(&self) -> u64 {
        // Wire deadlines use Unix milliseconds. Elapsed time remains monotonic
        // so a wall-clock adjustment cannot extend an outstanding approval.
        self.unix_ms
            .saturating_add(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }
}

fn workspace_limits() -> Limits {
    Limits {
        max_entries: 100_000,
        max_files: 50_000,
        max_file_bytes: 32 * 1024 * 1024,
        max_total_bytes: 512 * 1024 * 1024,
        max_depth: 64,
    }
}

fn answer_finalizer_resource(
    home: &Path,
    store_root: &Path,
    project_root: &Path,
    _executable: &Path,
    finalizer_decider: Option<Arc<dyn polaris_provider::decision::Decider>>,
) -> Result<Option<Arc<crate::shadow_finalizer::ShadowFinalizerOwner>>, StartupError> {
    use polaris_core::config::{AnswerFinalizerMode, PresentationLanguage, TranslationProfile};
    use polaris_core::translation::ManagedCatTranslate;

    let (presentation, finalizer) =
        polaris_core::config::try_load_answer_finalizer_with_managed_from(
            Some(&home.join(".polaris/managed-preferences.toml")),
            Some(&home.join(".polaris/config.toml")),
            Some(&project_root.join(".polaris/config.toml")),
        )
        .map_err(|_| StartupError::Generic)?;
    match finalizer.mode {
        AnswerFinalizerMode::Off => return Ok(None),
        // Configuration is only a request. No benchmark activation decision
        // exists in this startup contract, so enforce must fail closed.
        AnswerFinalizerMode::Enforce => return Err(StartupError::Generic),
        AnswerFinalizerMode::Shadow => {}
    }
    let translation_requested = finalizer.translation.activation.is_requested();
    let (translator, unavailable) = if !translation_requested {
        (None, None)
    } else if presentation.language != PresentationLanguage::Ja {
        (
            None,
            Some(crate::shadow_finalizer::ShadowFailureV1::PresentationLanguageNotJa),
        )
    } else if finalizer.translation.profile != Some(TranslationProfile::CatTranslate3_3b) {
        (
            None,
            Some(crate::shadow_finalizer::ShadowFailureV1::ModelUnavailable),
        )
    } else {
        match ManagedCatTranslate::open(home) {
            Ok(translator)
                if tokio::runtime::Handle::try_current()
                    .ok()
                    .is_some_and(|runtime| runtime.block_on(translator.preflight()).is_ok()) =>
            {
                (
                    Some(Arc::new(translator)
                        as Arc<dyn polaris_core::translation::TranslationBackendV1>),
                    None,
                )
            }
            _ => (
                None,
                Some(crate::shadow_finalizer::ShadowFailureV1::ModelUnavailable),
            ),
        }
    };
    crate::shadow_finalizer::ShadowFinalizerOwner::open_configured(
        store_root.join("answer-finalizer-shadow-v1.json"),
        presentation,
        finalizer,
        finalizer_decider,
        translator,
        unavailable,
    )
    .map_err(|_| StartupError::Generic)
}

/// The normal decision path may retry a transient Jev failure.  Shadow
/// finalization has a distinct disclosure receipt and may issue exactly one
/// physical request, so it receives an isolated one-shot client instead.
fn finalizer_decider_from_configuration(
    config: &polaris_core::config::DecisionConfig,
) -> Option<Arc<dyn polaris_provider::decision::Decider>> {
    polaris_core::decision_bands::build_configured(
        config,
        polaris_core::decision_bands::DecisionUse::Finalization,
    )
    .ok()
    .flatten()
}

fn catalog_copies(paths: &[PathBuf]) -> Result<Vec<Snapshot>, StartupError> {
    let mut copies = Vec::new();
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(m) if m.is_dir() => {}
            _ => return Err(StartupError::Generic),
        }
        let copy = isolated_workspace::protected_snapshot(
            path,
            Limits {
                max_entries: 2048,
                max_files: 1024,
                max_file_bytes: 256 * 1024,
                max_total_bytes: 4 * 1024 * 1024,
                max_depth: 16,
            },
        )
        .map_err(|_| StartupError::Generic)?;
        // Do not silently publish a partial catalog after excluding a schema,
        // a manifest alias or a registered credential.
        if !copy.exclusions.is_empty() {
            return Err(StartupError::Generic);
        }
        copies.push(copy);
    }
    Ok(copies)
}

fn private_audit(recovery: &PinnedRecoveryBase) -> Result<AuditLog, StartupError> {
    // The recovery directory was exclusively created, pinned and synchronized.
    // SAFETY: a retained directory FD, fixed component and newly owned file FD.
    let fd = unsafe {
        libc::openat(
            recovery.directory.as_raw_fd(),
            c"audit.jsonl".as_ptr(),
            libc::O_WRONLY
                | libc::O_APPEND
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(StartupError::Generic);
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.sync_all().map_err(|_| StartupError::Generic)?;
    recovery
        .directory
        .sync_all()
        .map_err(|_| StartupError::Generic)?;
    AuditLog::from_private_file(file).map_err(|_| StartupError::Generic)
}

/// Called on the startup blocking worker. The executable and home are obtained
/// from the OS, not argv or HOME; bootstrap is checked before runtime side effects.
pub fn start_owner(args: OwnerLaunchArguments) -> Result<StartedOwner, StartupError> {
    let home = trusted_user_home().map_err(|_| StartupError::Generic)?;
    polaris_auth::protection::use_trusted_home(&home).map_err(|_| StartupError::Generic)?;
    let executable =
        physical_executable(&std::env::current_exe().map_err(|_| StartupError::Generic)?);
    start_at(args, &home, &executable)
}

// LaunchServices may report its fixed /tmp and /var aliases. Expand only those
// OS aliases, then let the package reader verify every physical component with
// O_NOFOLLOW. Never canonicalize arbitrary package or helper symlinks.
fn physical_executable(path: &Path) -> PathBuf {
    for (alias, physical) in [("/tmp", "/private/tmp"), ("/var", "/private/var")] {
        if let Ok(suffix) = path.strip_prefix(alias) {
            return Path::new(physical).join(suffix);
        }
    }
    path.to_owned()
}

fn apply_native_decision_selection(
    decision: &mut polaris_core::config::DecisionConfig,
    selected: Result<String, std::env::VarError>,
) -> Result<(), StartupError> {
    match selected.as_deref() {
        Ok("1") => decision.enabled = true,
        Ok("0") => decision.enabled = false,
        Err(std::env::VarError::NotPresent) => {}
        _ => return Err(StartupError::Generic),
    }
    Ok(())
}

const MANAGED_RUST_VERSION: &str = "1.96.0";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedDependencyReceipt {
    schema_version: u32,
    id: String,
    version: String,
    manifest_sha256: String,
    #[serde(default)]
    runtime_tree_sha256: Option<String>,
    installed_at: String,
}

fn managed_rust_toolchain(
    home: &Path,
    executable: &Path,
) -> Result<Option<polaris_core::isolated_run::toolchains::RelocatablePackage>, StartupError> {
    let root = home
        .join(".polaris/toolchains/rust")
        .join(MANAGED_RUST_VERSION);
    match std::fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StartupError::Generic),
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(StartupError::Generic),
    }
    let app_root = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or(StartupError::Generic)?;
    let manifest_path =
        app_root.join("PolarisDesktop_PolarisDesktop.bundle/runtime-dependencies.json");
    let manifest_metadata =
        std::fs::symlink_metadata(&manifest_path).map_err(|_| StartupError::Generic)?;
    if !manifest_metadata.file_type().is_file()
        || manifest_metadata.file_type().is_symlink()
        || manifest_metadata.len() > 512 * 1024
    {
        return Err(StartupError::Generic);
    }
    let manifest = std::fs::read(&manifest_path).map_err(|_| StartupError::Generic)?;
    let document: serde_json::Value =
        serde_json::from_slice(&manifest).map_err(|_| StartupError::Generic)?;
    let architecture = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return Err(StartupError::Generic);
    };
    let matches = document
        .get("dependencies")
        .and_then(|value| value.as_array())
        .ok_or(StartupError::Generic)?
        .iter()
        .filter(|entry| {
            entry.get("id").and_then(|v| v.as_str()) == Some("rust")
                && entry.get("version").and_then(|v| v.as_str()) == Some(MANAGED_RUST_VERSION)
                && entry.get("platform").and_then(|v| v.as_str()) == Some("macos")
                && entry.get("architecture").and_then(|v| v.as_str()) == Some(architecture)
                && entry.get("required_executable").and_then(|v| v.as_str()) == Some("bin/rustc")
        })
        .count();
    if document.get("schema_version").and_then(|v| v.as_u64()) != Some(1) || matches != 1 {
        return Err(StartupError::Generic);
    }
    let manifest_hash = Sha256::digest(&manifest)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let receipt_path = root
        .parent()
        .ok_or(StartupError::Generic)?
        .join(format!(".{MANAGED_RUST_VERSION}.receipt.json"));
    let receipt_metadata =
        std::fs::symlink_metadata(&receipt_path).map_err(|_| StartupError::Generic)?;
    if !receipt_metadata.file_type().is_file()
        || receipt_metadata.file_type().is_symlink()
        || receipt_metadata.len() > 16 * 1024
    {
        return Err(StartupError::Generic);
    }
    let receipt: ManagedDependencyReceipt =
        serde_json::from_slice(&std::fs::read(&receipt_path).map_err(|_| StartupError::Generic)?)
            .map_err(|_| StartupError::Generic)?;
    if receipt.schema_version != 1
        || receipt.id != "rust"
        || receipt.version != MANAGED_RUST_VERSION
        || receipt.manifest_sha256 != manifest_hash
        || receipt.runtime_tree_sha256.is_some()
        || receipt.installed_at.is_empty()
        || receipt.installed_at.len() > 128
    {
        return Err(StartupError::Generic);
    }
    for path in [&root, &root.join("bin"), &root.join("bin/rustc")] {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| StartupError::Generic)?;
        if metadata.file_type().is_symlink()
            || !matches!(metadata.uid(), 0) && metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o002 != 0
        {
            return Err(StartupError::Generic);
        }
    }
    let rustc =
        std::fs::symlink_metadata(root.join("bin/rustc")).map_err(|_| StartupError::Generic)?;
    if !rustc.file_type().is_file() || rustc.permissions().mode() & 0o111 == 0 {
        return Err(StartupError::Generic);
    }
    Ok(Some(
        polaris_core::isolated_run::toolchains::RelocatablePackage {
            root,
            bin_dirs: vec![PathBuf::from("bin")],
        },
    ))
}

fn start_at(
    args: OwnerLaunchArguments,
    home: &Path,
    executable: &Path,
) -> Result<StartedOwner, StartupError> {
    let root =
        DesktopRoot::open_owned(&args.store.store_root).map_err(|_| StartupError::Generic)?;
    // M6-5: record new private copies here, and remove the ones a killed
    // process left behind. A sweep failure never stops the owner.
    polaris_core::scratch_registry::install(&args.store.store_root);
    let _ = polaris_core::scratch_registry::sweep(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        polaris_core::scratch_registry::RETENTION_SECONDS,
        |pid| {
            // SAFETY: signal 0 only tests for the process; it delivers nothing.
            unsafe { libc::kill(pid as i32, 0) == 0 }
        },
    );
    if root.identity().map_err(|_| StartupError::Generic)?
        != (
            args.store_identity.device.get(),
            args.store_identity.inode.get(),
        )
    {
        return Err(StartupError::Generic);
    }
    let writer = root
        .open(&args.store.project_id, &args.store.session_id)
        .map_err(|_| StartupError::Generic)?;
    let saved = writer.snapshot().map_err(|_| StartupError::Generic)?;
    let bootstrap = read_owner_bootstrap(
        &args.store.store_root.join(&args.bootstrap_name),
        &args.bootstrap_file,
        &ExpectedBootstrapStore {
            path: &args.store.store_root,
            identity: args.store_identity,
        },
        &saved,
    )
    .map_err(|_| StartupError::Generic)?;
    let doc = bootstrap.document();
    if args.confirmed_source_path != Path::new(&doc.source_path)
        || args.confirmed_source_identity != doc.source_identity
        || args.confirmed_tier != doc.tier
        || args.confirmed_policy_revision != doc.policy_revision
    {
        return Err(StartupError::Generic);
    }
    // Reject unsupported/missing memory resources before local model discovery
    // or any credential-backed client construction. Configuration-save services
    // never enter this production-only startup path.
    let mcp_store = polaris_mcp::store::Store::open(home).map_err(|_| StartupError::Generic)?;
    // A source that cannot be registered is not a regular file and is never read.
    let _ = polaris_core::mcp::setup::protect_sources(home);
    let mcp_snapshot = mcp_store
        .load_snapshot()
        .map_err(|_| StartupError::Generic)?;
    let mcp_active = (
        mcp_snapshot.registry.as_ref().map_or(0, |r| r.revision),
        mcp_snapshot.secrets.as_ref().map_or(0, |s| s.revision()),
    );
    let history = crate::memory_resources::prepare(
        saved.state.configuration.history_mode,
        doc.provider,
        home,
        &args.store.store_root,
        &args.confirmed_source_path,
    )
    .map_err(StartupError::Memory)?;
    // These existing loaders register the actual credential inode before any
    // model-visible catalog or workspace read. Never inspect their values in logs.
    let backend = match doc.provider {
        // Multiple accounts take over completely: the legacy single
        // authentication is neither read nor used to fill a gap.
        BootstrapProvider::Codex => {
            match polaris_auth::accounts::owner::AccountOwner::open_if_registered(
                polaris_auth::ISSUER,
            )
            .map_err(|_| StartupError::Generic)?
            {
                Some(owner) => TrustedBackend::Codex {
                    tokens: Arc::new(crate::auth_tokens::UnboundAccountTokens),
                    accounts: Some(owner),
                },
                None => TrustedBackend::Codex {
                    tokens: Arc::new(
                        DesktopAuthTokens::from_trusted_store(home.join(".polaris/auth.json"))
                            .map_err(|_| StartupError::Generic)?,
                    ),
                    accounts: None,
                },
            }
        }
        BootstrapProvider::Openai => TrustedBackend::Openai {
            api_key: polaris_auth::api_key::load_from(&home.join(".polaris/api_key.json"))
                .map_err(|_| StartupError::Generic)?
                .ok_or(StartupError::Generic)?,
        },
        BootstrapProvider::Ollama | BootstrapProvider::Lmstudio => {
            let runtime = if doc.provider == BootstrapProvider::Ollama {
                Runtime::Ollama
            } else {
                Runtime::LmStudio
            };
            let adapter = LocalAdapter::new(
                runtime,
                Endpoint::parse(doc.local_endpoint.as_deref().ok_or(StartupError::Generic)?)
                    .map_err(|_| StartupError::Generic)?,
            )
            .map_err(|_| StartupError::Generic)?;
            let selection = tokio::runtime::Handle::current().block_on(async {
                let inventory = adapter
                    .inventory()
                    .await
                    .map_err(|_| StartupError::Generic)?;
                let model = inventory
                    .iter()
                    .find(|m| m.id() == bootstrap.model())
                    .ok_or(StartupError::Generic)?;
                adapter
                    .select(model)
                    .await
                    .map_err(|_| StartupError::Generic)
            })?;
            TrustedBackend::Local {
                selection,
                router: Arc::new(LocalRouter::new(8)),
            }
        }
    };
    let helper = read_package_manifest(executable).map_err(|_| StartupError::Generic)?;
    let runtime_roots = ["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
        .map(PathBuf::from)
        .to_vec();
    let mcp_runtime_roots = runtime_roots.clone();
    let mcp_excluded_source = args.confirmed_source_path.clone();
    let mcp_excluded_store = args.store.store_root.clone();
    let mcp = polaris_core::mcp::runtime::RuntimeConfig::prepare_registered(
        mcp_snapshot,
        args.store.project_id.as_str().into(),
        home,
        &runtime_roots,
        &[
            args.confirmed_source_path.clone(),
            args.store.store_root.clone(),
        ],
        workspace_limits(),
    )
    .map_err(|_| StartupError::Generic)?;
    let resources_root = executable
        .parent()
        .and_then(Path::parent)
        .ok_or(StartupError::Generic)?
        .join("Resources");
    let mut skill_copies = catalog_copies(&[
        args.confirmed_source_path.join(".polaris/skills"),
        home.join(".polaris/skills"),
        resources_root.join("skills"),
    ])?;
    let mut agent_copies = catalog_copies(&[
        args.confirmed_source_path.join("agents"),
        home.join(".polaris/agents"),
        resources_root.join("agents"),
    ])?;
    let skills = polaris_skills::discover_in(
        &skill_copies
            .iter()
            .map(|s| s.path().to_owned())
            .collect::<Vec<_>>(),
    );
    let agents = polaris_skills::discover_agent_types_in(
        &agent_copies
            .iter()
            .map(|s| s.path().to_owned())
            .collect::<Vec<_>>(),
    );
    if skills.skills.is_empty()
        || agents.agent_types.is_empty()
        || !skills.skipped.is_empty()
        || !agents.skipped.is_empty()
        || agents
            .agent_types
            .iter()
            .any(|a| !a.output_schema.is_file())
    {
        return Err(StartupError::Generic);
    }
    // Saved workflow configuration is authoritative on reconnect. Only a new
    // owner reads workflow fields from the pinned global/project config paths.
    let runtime_workflow = match &saved.state.workflow {
        Some(workflow) => workflow
            .restore_with_artifacts(None, saved.state.workflow_artifacts.clone())
            .map_err(|_| StartupError::Generic)?,
        None => polaris_core::workflow::SessionWorkflow::try_new(
            polaris_core::config::try_load_workflow_from(
                Some(&home.join(".polaris/config.toml")),
                Some(&args.confirmed_source_path.join(".polaris/config.toml")),
            )
            .map_err(|_| StartupError::Generic)?,
        )
        .map_err(|_| StartupError::Generic)?,
    };
    let workflow = runtime_workflow.config.clone();
    let input_preparation = match polaris_core::config::try_load_input_preparation_from(
        Some(&home.join(".polaris/config.toml")),
        Some(&args.confirmed_source_path.join(".polaris/config.toml")),
    ) {
        Ok(mode) => mode,
        // A reconnect already has authoritative saved state. An unreadable
        // optional optimization must retain its original-input request path.
        Err(_) if saved.state.workflow.is_some() => Default::default(),
        Err(_) => return Err(StartupError::Generic),
    };
    // The same `[decision]` table the terminal reads, from the same two
    // pinned paths. The key stays in the environment: a configuration file
    // is shared, a credential is not.
    // An invalid optional decision configuration leaves this path without a
    // judge. Saved workflow restoration and mandatory finalizer configuration
    // validation remain separate; malformed TOML can still stop startup there.
    let mut decision = polaris_core::config::try_load_decision_from(
        Some(&home.join(".polaris/config.toml")),
        Some(&args.confirmed_source_path.join(".polaris/config.toml")),
    )
    .unwrap_or_default();
    // The native setup selection is explicit and overrides the shareable TOML
    // for this owner. The key itself remains outside every config and argv.
    apply_native_decision_selection(&mut decision, std::env::var("POLARIS_JEV_ENABLED"))?;
    if decision.enabled
        && let Some(fallback) = &decision.fallback
    {
        for line in fallback.disclosure(&decision) {
            eprintln!("{line}");
        }
    }
    let decider = polaris_core::decision_bands::from_configuration(&decision);
    let finalizer_decider = finalizer_decider_from_configuration(&decision);
    if workflow.enabled && runtime_workflow.begin_turn(&skills.skills).is_err() {
        return Err(StartupError::Generic);
    }
    let always_on = assemble_always_on("", "macOS desktop; isolated workspace", &skills.skills)
        .with_agents_refresh(
            AgentsRefresh::new(
                Some(&home.join(".polaris/AGENTS.md")),
                &args.confirmed_source_path,
            )
            .map_err(|_| StartupError::Generic)?,
        )
        .map_err(|_| StartupError::Generic)?;
    let source = CurrentSourcePolicy {
        source_path: args.confirmed_source_path.clone(),
        source_identity: SourceApplyIdentity {
            device: doc.source_identity.device,
            inode: doc.source_identity.inode,
        },
        policy_revision: doc.policy_revision,
        read_allowed: true,
        write_allowed: doc.tier != BootstrapTier::ReadOnly,
    };
    // Build tiers require the exact Polaris-managed toolchain. Never adopt a
    // host PATH toolchain or bytes copied into the application package.
    let packages = managed_rust_toolchain(home, executable)?
        .into_iter()
        .collect::<Vec<_>>();
    if packages.is_empty()
        && matches!(
            doc.tier,
            BootstrapTier::ReadCreateBuild | BootstrapTier::ReadCreateBuildExternal
        )
    {
        return Err(StartupError::Generic);
    }
    let toolchains =
        (!packages.is_empty()).then_some(polaris_core::isolated_run::toolchains::ToolchainPlan {
            packages,
            limits: polaris_core::isolated_run::toolchains::RuntimeLimits {
                max_packages: 8,
                max_bin_dirs: 8,
                copy: Limits {
                    max_file_bytes: 256 * 1024 * 1024,
                    max_total_bytes: 1024 * 1024 * 1024,
                    ..workspace_limits()
                },
            },
        });
    let recovery = prepare_recovery_base(
        &source,
        &root,
        &args.store.store_root,
        &args.store.project_id,
        &args.store.session_id,
    )
    .map_err(|_| StartupError::Generic)?;
    let answer_finalizer = answer_finalizer_resource(
        home,
        &args.store.store_root,
        &args.confirmed_source_path,
        executable,
        finalizer_decider,
    )?;
    let audit = Arc::new(tokio::sync::Mutex::new(private_audit(&recovery)?));
    let meter = polaris_provider::decision::DecisionMeter::default();
    polaris_core::audit::install_decision_observer(&meter, audit.clone());
    let decider = decider.map(|decider| meter.wrap(decider));
    let resources = TrustedFactoryResources {
        decision_local_only: decision.local_only,
        always_on,
        audit,
        skills: skills.skills,
        agent_types: agents.agent_types,
        max_turns: 64,
        spawn_concurrency: 4,
        spawn_write_concurrency: 1,
        tool_memory: None,
        history,
        mcp,
        workflow,
        input_preparation,
        decider,
        answer_finalizer,
    };
    let limits = OwnerLaunchLimits {
        workspace: workspace_limits(),
        source: workspace_limits(),
        runtime_roots,
        toolchains,
        copy_mutations: source.write_allowed,
        clock: Arc::new(Clock::new()?),
        approval_ttl_ms: 300_000,
    };
    let mut service = compose_existing(
        args, root, writer, helper, backend, resources, recovery, limits,
    )
    .map_err(|_| StartupError::Generic)?;
    service.attach_mcp_settings(crate::engine::McpSettings {
        store: mcp_store,
        home: home.to_owned(),
        active_registry_revision: mcp_active.0,
        active_secrets_revision: mcp_active.1,
        runtime_roots: mcp_runtime_roots,
        excluded_roots: vec![mcp_excluded_source, mcp_excluded_store],
        limits: workspace_limits(),
    });
    skill_copies.append(&mut agent_copies);
    Ok(StartedOwner {
        service,
        _catalogs: skill_copies,
    })
}

#[cfg(test)]
mod tests;
