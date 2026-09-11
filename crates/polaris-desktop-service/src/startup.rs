//! Production composition, owned until both IPC channels and engine have drained.
//! No environment-selected credentials, catalogs, executable or permission grants.
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
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
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

fn start_at(
    args: OwnerLaunchArguments,
    home: &Path,
    executable: &Path,
) -> Result<StartedOwner, StartupError> {
    let root =
        DesktopRoot::open_owned(&args.store.store_root).map_err(|_| StartupError::Generic)?;
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
        BootstrapProvider::Codex => TrustedBackend::Codex {
            tokens: Arc::new(
                DesktopAuthTokens::from_trusted_store(home.join(".polaris/auth.json"))
                    .map_err(|_| StartupError::Generic)?,
            ),
        },
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
    // Build tiers require actual relocatable packages; never turn an empty
    // toolchain into a successful build capability or allow a host-wide fallback.
    let packages_root = resources_root.join("toolchains");
    let mut packages = Vec::new();
    if packages_root.is_dir() {
        for entry in std::fs::read_dir(&packages_root).map_err(|_| StartupError::Generic)? {
            let entry = entry.map_err(|_| StartupError::Generic)?;
            if !entry
                .file_type()
                .map_err(|_| StartupError::Generic)?
                .is_dir()
            {
                return Err(StartupError::Generic);
            }
            packages.push(polaris_core::isolated_run::toolchains::RelocatablePackage {
                root: entry.path(),
                bin_dirs: vec![PathBuf::from("bin")],
            });
        }
        packages.sort_by(|a, b| a.root.cmp(&b.root));
    }
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
    let resources = TrustedFactoryResources {
        always_on,
        audit: Arc::new(tokio::sync::Mutex::new(private_audit(&recovery)?)),
        skills: skills.skills,
        agent_types: agents.agent_types,
        max_turns: 64,
        spawn_concurrency: 4,
        spawn_write_concurrency: 1,
        tool_memory: None,
        history,
    };
    let limits = OwnerLaunchLimits {
        workspace: workspace_limits(),
        source: workspace_limits(),
        runtime_roots: ["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
            .map(PathBuf::from)
            .to_vec(),
        toolchains,
        copy_mutations: source.write_allowed,
        clock: Arc::new(Clock::new()?),
        approval_ttl_ms: 300_000,
    };
    let service = compose_existing(
        args, root, writer, helper, backend, resources, recovery, limits,
    )
    .map_err(|_| StartupError::Generic)?;
    skill_copies.append(&mut agent_copies);
    Ok(StartedOwner {
        service,
        _catalogs: skill_copies,
    })
}

#[cfg(test)]
mod tests;
