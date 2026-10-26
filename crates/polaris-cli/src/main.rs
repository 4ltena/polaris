//! Entry point for the `polaris` binary. Decides the endpoint from environment
//! variables, assembles the always-on context, and runs the agent loop once.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use polaris_core::{
    agent,
    agent::ToolContext,
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    constitution, prompt,
    session::Session,
    stop::StopTracker,
    workflow::Phase,
};
use polaris_provider::openai::OpenAiProvider;
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde::Deserialize;

mod account;
#[cfg(unix)]
mod admission;
mod benchmark_report;
mod compare;
mod decided;
mod decision;
mod env;
mod handoff;
mod mcp;
mod memory;
#[cfg(not(windows))]
mod provider_startup;
mod setting;
mod spare;
#[cfg(test)]
mod strict_history_tests;
mod sync;

#[derive(Parser)]
#[command(
    name = "polaris",
    version,
    about = "A minimal-context coding agent",
    after_help = "\
Environment variables:
  POLARIS_PROVIDER  Auto-select openai/codex from credentials; or set openai, codex, or llamacpp.
  POLARIS_API_KEY   Required when provider=openai. API key for an OpenAI-compatible endpoint.
  POLARIS_BASE_URL  Used when provider=openai; defaults to https://api.openai.com/v1
  POLARIS_MODEL     TUI defaults to gpt-6.1-sol; one-shot defaults to gpt-6-astra
"
)]
struct Args {
    /// The instruction to run. Not needed for subcommands or `--confined-apply`.
    ///
    /// We don't use clap's `required_unless_present` because it only looks at
    /// argument names, not whether a subcommand is present. Making this
    /// required here would make `polaris login` get rejected for "missing
    /// --prompt". In M2 Task 8, `--confined-apply` became unreachable the
    /// same way. Validation is done by hand after parsing.
    #[arg(short, long)]
    prompt: Option<String>,

    /// Where to write the audit log. Defaults to `~/.polaris/state/<project-id>/audit.jsonl` when omitted.
    #[arg(long)]
    audit: Option<PathBuf>,

    /// The maximum number of turns allowed in a single run.
    #[arg(long, default_value_t = 20)]
    max_turns: u32,

    /// Model to run. Falls back to POLARIS_MODEL, then gpt-6.1-sol for TUI or gpt-6-astra for one-shot runs.
    #[arg(long)]
    model: Option<String>,

    /// Registration ID used for sends. If omitted, use the only registration or select among multiple registrations using usage limits.
    #[arg(long)]
    account: Option<String>,

    /// Reasoning effort. Automatic in interactive TUI; defaults to xhigh with `-p`.
    ///
    /// This default applies to the root orchestrator run. Delegated effort is set by `[tier.*]`; only tiers without an effort inherit this value. `max` requires the Responses API, which Polaris uses. `ultra` is not accepted because it is not an API value.
    #[arg(long, value_parser = ["low", "medium", "high", "xhigh", "max"])]
    effort: Option<String>,

    /// Save pre-compaction history locally and build a per-project search index.
    #[arg(long)]
    remember: bool,

    /// Tool-result retention; defaults to off, independently of --remember history storage.
    #[arg(long, value_enum, default_value_t = ToolMemoryArg::Off)]
    tool_memory: ToolMemoryArg,

    /// Tool-memory embedding URL. Requires a model and enabled tool memory.
    #[arg(long, requires = "tool_memory_embedding_model")]
    tool_memory_embedding_url: Option<String>,

    /// Tool-memory embedding model. Requires an embedding URL.
    #[arg(long, requires = "tool_memory_embedding_url")]
    tool_memory_embedding_model: Option<String>,

    /// Estimated token threshold for automatic compaction; defaults to 200,000.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1000..))]
    compact_at: Option<u32>,

    /// Hosted web search. cached/live are refused until the endpoint capability contract is verified.
    #[arg(long, value_enum, default_value_t = WebSearchArg::Disabled)]
    web_search: WebSearchArg,

    /// Work phase for a workflow-enabled session.
    #[arg(long, value_enum, conflicts_with_all = ["resume", "fork"])]
    phase: Option<PhaseArg>,

    /// Host-authored routine-change authority for this exact prompt.
    #[arg(long, value_name = "JSON")]
    routine_authority: Option<String>,

    /// Host-authored review for the exact published verification checkpoint.
    #[arg(long, value_name = "JSON", requires = "routine_authority")]
    review_authority: Option<String>,

    /// Explicitly change the saved workflow mode. On resume/fork, apply after saving the change.
    #[arg(long, value_enum)]
    workflow_mode: Option<WorkflowModeArg>,

    /// History storage mode. strict10 requires pinned local embedding resources.
    #[arg(long, value_enum)]
    history_mode: Option<HistoryModeArg>,

    /// Resume a saved v2 session. Legacy JSONL is imported once into a new v2 UUID.
    #[arg(long, conflicts_with = "fork")]
    resume: Option<String>,

    /// Fork the specified v2 session into a new UUID and run it.
    #[arg(long, conflicts_with = "resume")]
    fork: Option<String>,

    /// Run as a confined child that reads one mutation operation from stdin
    /// and executes it. Internal use only; not meant to be invoked directly
    /// by users.
    #[arg(long, hide = true)]
    confined_apply: bool,

    /// Internal entry point for one bounded read in an OS-confined child.
    #[arg(long, hide = true, conflicts_with = "confined_apply")]
    confined_read: bool,

    /// The sandbox policy.
    #[arg(long, value_enum, default_value_t = SandboxModeArg::WorkspaceWrite)]
    sandbox: SandboxModeArg,

    /// The approval boundary policy.
    #[arg(long, value_enum, default_value_t = ApprovalPolicyArg::OnRequest)]
    approval: ApprovalPolicyArg,

    /// Allow MCP tool calls and registration, with confirmation for each operation.
    #[arg(long)]
    mcp_external: bool,

    /// Select one MCP server prompt as `server/prompt` before execution. Selection is allowed only once, before the first request.
    #[arg(long, value_name = "SERVER/PROMPT")]
    mcp_prompt: Option<String>,

    /// Argument for --mcp-prompt as key=value. May be repeated.
    #[arg(long = "mcp-prompt-arg", value_name = "KEY=VALUE")]
    mcp_prompt_args: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutineAuthorityWireV1 {
    schema_version: u32,
    component_root: PathBuf,
    security: bool,
    migration: bool,
    public_api: bool,
    dependency: bool,
    distribution: bool,
    major_user_contract: bool,
    unknown_impact: bool,
    criteria: Vec<RoutineCriterionWireV1>,
    #[serde(default)]
    existing_test: Option<ExistingTestAuthorityWireV1>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutineCriterionWireV1 {
    description: String,
    observable_failure: String,
    evidence_kind: RoutineCriterionEvidenceWireV1,
    #[serde(default)]
    common_runner_plan: Option<polaris_core::common_tests::Plan>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RoutineCriterionEvidenceWireV1 {
    Static,
    CommonCase,
    ExistingTest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExistingTestAuthorityWireV1 {
    cargo_program: PathBuf,
    timeout_secs: u64,
    inventory: polaris_core::verification::TestInventoryV1,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewAuthorityWireV1 {
    schema_version: u32,
    expected_workflow_revision: u64,
    checkpoint_id: String,
    reviewer_id: String,
    findings: Vec<polaris_core::workflow::ReviewFindingV1>,
}

#[derive(Clone)]
struct PreparedRoutineAuthorityV1 {
    eligibility: polaris_core::workflow::RoutineEligibilityV1,
    verification: Vec<polaris_core::workflow::VerificationCriterionV1>,
    runners: Vec<polaris_core::verification::FixedCommonRunnerV1>,
    existing_test_executor: Option<polaris_core::verification::FixedRustTestExecutorV1>,
    static_adapters: Vec<polaris_core::verification::FixedStaticAdapterV1>,
    verification_preparation: polaris_core::verification::HostVerificationPreparationV1,
    component_root: PathBuf,
    subject_hash: String,
}

struct CliVerificationObserverV1 {
    project_root: PathBuf,
    component_root: PathBuf,
    subject_hash: String,
    cargo_program: PathBuf,
}

impl polaris_core::verification::VerificationObserverV1 for CliVerificationObserverV1 {
    fn observe(&self) -> Result<polaris_core::verification::HostObservedVerificationV1, String> {
        let (source_hash, project_graph_hash) = cargo_closure_hashes(
            &self.project_root,
            &self.component_root,
            &self.cargo_program,
        )?;
        Ok(polaris_core::verification::HostObservedVerificationV1 {
            subject_hash: self.subject_hash.clone(),
            source_hash: source_hash.clone(),
            diff_hash: source_hash,
            project_graph_hash,
        })
    }
}

#[derive(Deserialize)]
struct CargoMetadataBindingV1 {
    packages: Vec<CargoMetadataPackageV1>,
    resolve: Option<CargoMetadataResolveV1>,
    workspace_root: String,
}

#[derive(Deserialize)]
struct CargoMetadataPackageV1 {
    id: String,
    manifest_path: String,
    source: Option<String>,
}

#[derive(Deserialize)]
struct CargoMetadataResolveV1 {
    nodes: Vec<CargoMetadataNodeV1>,
}

#[derive(Deserialize)]
struct CargoMetadataNodeV1 {
    id: String,
    dependencies: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ToolMemoryArg {
    Off,
    History,
    Retrieval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WebSearchArg {
    Disabled,
    Cached,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum PhaseArg {
    General,
    Brainstorm,
    Specify,
    Implement,
    Review,
    Verify,
    Deliver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WorkflowModeArg {
    Manual,
    Adaptive,
    Off,
}

impl WorkflowModeArg {
    fn apply(self, config: &mut polaris_core::workflow::WorkflowConfig) {
        match self {
            Self::Manual => {
                config.enabled = true;
                config.mode = polaris_core::workflow::WorkflowMode::Manual;
            }
            Self::Adaptive => {
                config.enabled = true;
                config.mode = polaris_core::workflow::WorkflowMode::Adaptive;
            }
            Self::Off => {
                config.enabled = false;
                config.mode = polaris_core::workflow::WorkflowMode::Manual;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum HistoryModeArg {
    Legacy,
    Strict10,
    Structured10,
}

impl From<HistoryModeArg> for polaris_core::conversation_state::HistoryMode {
    fn from(value: HistoryModeArg) -> Self {
        match value {
            HistoryModeArg::Legacy => Self::Legacy,
            HistoryModeArg::Strict10 => Self::Strict10,
            HistoryModeArg::Structured10 => Self::Structured10,
        }
    }
}

impl From<PhaseArg> for Phase {
    fn from(value: PhaseArg) -> Self {
        match value {
            PhaseArg::General => Self::General,
            PhaseArg::Brainstorm => Self::Brainstorm,
            PhaseArg::Specify => Self::Specify,
            PhaseArg::Implement => Self::Implement,
            PhaseArg::Review => Self::Review,
            PhaseArg::Verify => Self::Verify,
            PhaseArg::Deliver => Self::Deliver,
        }
    }
}

fn prepare_routine_authority(
    raw: &str,
    prompt: &str,
    project_root: &Path,
    sandbox: &SandboxPolicy,
    static_state_root: &Path,
) -> Result<PreparedRoutineAuthorityV1, String> {
    let wire: RoutineAuthorityWireV1 = serde_json::from_str(raw)
        .map_err(|error| format!("Invalid --routine-authority: {error}"))?;
    if wire.schema_version != 1 {
        return Err("--routine-authority accepts only schema_version 1".into());
    }
    if sandbox.mode() != SandboxMode::WorkspaceWrite {
        return Err("--routine-authority requires a workspace-write sandbox".into());
    }
    if wire.component_root.as_os_str().is_empty()
        || wire.component_root.is_absolute()
        || wire.component_root.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err("Specify component_root as a relative path within the project".into());
    }
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("Cannot verify the project root: {error}"))?;
    let component_root = project_root
        .join(&wire.component_root)
        .canonicalize()
        .map_err(|error| format!("Cannot verify component_root: {error}"))?;
    if component_root == project_root || !component_root.starts_with(&project_root) {
        return Err("component_root must be the root of a single package".into());
    }
    if !sandbox
        .writable_roots()
        .iter()
        .any(|root| component_root.starts_with(root))
    {
        return Err("component_root is outside the sandbox write roots".into());
    }
    let manifest_path = component_root.join("Cargo.toml");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("Cannot read the component manifest: {error}"))?;
    let manifest: toml::Value = toml::from_str(&manifest_text)
        .map_err(|error| format!("Invalid component manifest: {error}"))?;
    let package_name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .ok_or("component_root requires a single Cargo package")?
        .to_owned();

    let component = wire
        .component_root
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let eligibility = polaris_core::workflow::RoutineEligibilityV1 {
        affected_components: vec![component],
        security: wire.security,
        migration: wire.migration,
        public_api: wire.public_api,
        dependency: wire.dependency,
        distribution: wire.distribution,
        major_user_contract: wire.major_user_contract,
        unknown_impact: wire.unknown_impact,
    };
    eligibility.validate()?;
    if eligibility.blocker().is_some() {
        return Err("--routine-authority is restricted to routine changes".into());
    }
    if wire.criteria.is_empty() || wire.criteria.len() > 32 {
        return Err("Invalid --routine-authority criteria count".into());
    }
    let mut verification = Vec::with_capacity(wire.criteria.len());
    let mut runner_inputs = Vec::with_capacity(wire.criteria.len());
    let mut static_criteria = Vec::new();
    let mut has_existing_test = false;
    for criterion in wire.criteria {
        let (evidence_kind, common_runner) = match criterion.evidence_kind {
            RoutineCriterionEvidenceWireV1::Static => {
                if criterion.description != STATIC_COMPONENT_MANIFEST_DESCRIPTION
                    || criterion.observable_failure != STATIC_COMPONENT_MANIFEST_FAILURE
                    || criterion.common_runner_plan.is_some()
                    || criterion.timeout_secs.is_some()
                {
                    return Err(
                        "Static criteria accept only registered component manifest contracts"
                            .into(),
                    );
                }
                (
                    polaris_core::workflow::CriterionEvidenceKindV1::Static,
                    None,
                )
            }
            RoutineCriterionEvidenceWireV1::CommonCase => {
                let plan = criterion
                    .common_runner_plan
                    .ok_or("A common_case criterion requires common_runner_plan")?;
                plan.check()
                    .map_err(|error| format!("Invalid common runner plan: {error:?}"))?;
                let timeout_secs = criterion
                    .timeout_secs
                    .filter(|seconds| (1..=300).contains(seconds))
                    .ok_or("Specify a common runner timeout of 1..=300 seconds")?;
                (
                    polaris_core::workflow::CriterionEvidenceKindV1::CommonCase,
                    Some((plan, timeout_secs)),
                )
            }
            RoutineCriterionEvidenceWireV1::ExistingTest => {
                if criterion.common_runner_plan.is_some() || criterion.timeout_secs.is_some() {
                    return Err(
                        "An existing_test criterion cannot specify common runner settings".into(),
                    );
                }
                has_existing_test = true;
                (
                    polaris_core::workflow::CriterionEvidenceKindV1::ExistingTest,
                    None,
                )
            }
        };
        let typed = polaris_core::workflow::VerificationCriterionV1::typed(
            criterion.description,
            criterion.observable_failure,
            true,
            [evidence_kind].into_iter().collect(),
        )?;
        if let Some((plan, timeout_secs)) = common_runner {
            runner_inputs.push((typed.id.clone(), plan, timeout_secs));
        }
        if matches!(
            criterion.evidence_kind,
            RoutineCriterionEvidenceWireV1::Static
        ) {
            static_criteria.push(typed.id.clone());
        }
        verification.push(typed);
    }
    let (existing_test_executor, verification_preparation) = match (
        has_existing_test,
        wire.existing_test,
    ) {
        (false, None) => (
            None,
            polaris_core::verification::HostVerificationPreparationV1::default(),
        ),
        (false, Some(_)) => {
            return Err(
                "An executor cannot be registered without an existing_test criterion".into(),
            );
        }
        (true, None) => {
            return Err("An existing_test criterion requires a host executor".into());
        }
        (true, Some(config)) => {
            if !config.cargo_program.is_absolute() || !(1..=300).contains(&config.timeout_secs) {
                return Err("Invalid existing-test executor configuration".into());
            }
            config
                .inventory
                .validate()
                .map_err(|_| "Invalid existing-test inventory")?;
            if !config.inventory.complete
                || config.inventory.framework != "rust"
                || config
                    .inventory
                    .entries
                    .iter()
                    .any(|entry| entry.package != package_name)
            {
                return Err(
                        "The existing-test inventory must be a complete Rust inventory for one component".into(),
                    );
            }
            let mut candidate_hints = polaris_core::verification::CandidateHintsV1::default();
            candidate_hints.packages.insert(package_name.clone());
            let preparation = polaris_core::verification::HostVerificationPreparationV1 {
                inventory: Some(config.inventory),
                candidate_hints,
                all_tests_scope: polaris_core::verification::AllTestsScopeV1::Package,
                ..Default::default()
            };
            let executor = polaris_core::verification::FixedRustTestExecutorV1 {
                runner_id: format!("rust-tests-{package_name}"),
                cargo_program: config.cargo_program,
                policy: sandbox.clone(),
                cwd: project_root.clone(),
                write_root: project_root.clone(),
                timeout: Duration::from_secs(config.timeout_secs),
            };
            (Some(executor), preparation)
        }
    };
    let authorization = polaris_core::session::RoutineChangeAuthorizationV1::new(
        prompt,
        eligibility.clone(),
        verification.clone(),
    )?;
    drop(authorization);

    let runners = runner_inputs
        .into_iter()
        .map(
            |(criterion_id, plan, timeout_secs)| polaris_core::verification::FixedCommonRunnerV1 {
                runner_id: format!("common-{criterion_id}"),
                criterion_id,
                evidence_kind: polaris_core::verification::EvidenceKindV1::CommonCase,
                plan,
                policy: sandbox.clone(),
                cwd: component_root.clone(),
                write_root: component_root.clone(),
                timeout: Duration::from_secs(timeout_secs),
            },
        )
        .collect::<Vec<_>>();
    let static_adapters = build_component_manifest_static_adapters(
        &static_criteria,
        &component_root,
        static_state_root,
    )?;
    let subject_hash = polaris_core::conversation_state::content_hash(
        serde_json::to_string(&serde_json::json!({
            "eligibility": eligibility,
            "verification": verification,
        }))
        .map_err(|_| "Cannot hash routine authority")?
        .as_bytes(),
    );
    Ok(PreparedRoutineAuthorityV1 {
        eligibility,
        verification,
        runners,
        existing_test_executor,
        static_adapters,
        verification_preparation,
        component_root,
        subject_hash,
    })
}

fn build_component_manifest_static_adapters(
    criteria: &[String],
    component_root: &Path,
    static_state_root: &Path,
) -> Result<Vec<polaris_core::verification::FixedStaticAdapterV1>, String> {
    if criteria.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let entry_lease = production_entry_lease()
        .map_err(|()| "Cannot verify static analyzer entry admission".to_owned())?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let entry_files = entry_lease.isolated_read_files();
    let program = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("Cannot verify the static analyzer executable: {error}"))?;
    let program_bytes = std::fs::read(&program)
        .map_err(|error| format!("Cannot read the static analyzer executable: {error}"))?;
    let executable_hash = polaris_core::conversation_state::content_hash(&program_bytes);
    let mut runtime_roots = Vec::new();
    if let Some(parent) = program.parent() {
        runtime_roots.push(parent.to_path_buf());
    }
    for candidate in ["/usr/bin", "/bin", "/usr/lib", "/System/Library"] {
        let path = Path::new(candidate);
        if path.is_dir() {
            let canonical = path
                .canonicalize()
                .map_err(|error| format!("Cannot verify the static analyzer runtime: {error}"))?;
            if !runtime_roots.contains(&canonical) {
                runtime_roots.push(canonical);
            }
        }
    }
    let state_root = static_state_root
        .canonicalize()
        .map_err(|error| format!("Cannot verify the static analyzer state root: {error}"))?;
    let component_key =
        polaris_core::conversation_state::content_hash(component_root.to_string_lossy().as_bytes());
    let container = state_root.join(format!("static-adapter-{}", &component_key[..16]));
    let home = container.join("home");
    let tmpdir = container.join("tmp");
    prepare_static_private_directory(&container)?;
    prepare_static_private_directory(&home)?;
    prepare_static_private_directory(&tmpdir)?;
    let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, component_root, &runtime_roots);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let policy = policy.and_then(|policy| policy.with_isolated_readable_files(&entry_files));
    let policy = policy
        .and_then(|policy| policy.with_isolated_private_environment(&container, &home, &tmpdir))
        .and_then(SandboxPolicy::require_process_containment)
        .map_err(|error| format!("Cannot prepare the static analyzer sandbox: {error}"))?;
    criteria
        .iter()
        .map(|criterion_id| {
            let analyzer_id = format!("component-manifest-{criterion_id}");
            let capability = polaris_core::verification::StaticCapabilityV1 {
                schema_version: polaris_core::verification::SCHEMA_VERSION,
                analyzer_id,
                executable_hash: executable_hash.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                output_schema_hash:
                    polaris_core::verification::static_adapter_output_schema_hash_v1(),
                rules: vec![polaris_core::verification::StaticRuleCapabilityV1 {
                    rule_id: STATIC_COMPONENT_MANIFEST_RULE.into(),
                    obligation_kinds: ["criterion".into()].into_iter().collect(),
                    maximum_scope: polaris_core::verification::AnalysisScopeV1::Package,
                    clean_proves_satisfied: true,
                    requires_project_graph: false,
                }],
            };
            Ok(polaris_core::verification::FixedStaticAdapterV1 {
                criterion_id: criterion_id.clone(),
                runner_id: format!("static-component-manifest-{criterion_id}"),
                capability,
                rule_id: STATIC_COMPONENT_MANIFEST_RULE.into(),
                program: program.clone(),
                argv: vec!["static-component-manifest".into()],
                policy: policy.clone(),
                cwd: component_root.to_path_buf(),
                timeout: Duration::from_secs(10),
                maximum_output_bytes: 8 * 1024,
            })
        })
        .collect()
}

fn prepare_static_private_directory(path: &Path) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Err("Private static analyzer scratch space is unavailable on this platform".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "Cannot create private static analyzer scratch space: {error}"
                ));
            }
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| format!("Cannot protect private static analyzer scratch space: {error}"),
        )?;
        let metadata = path.symlink_metadata().map_err(|error| {
            format!("Cannot verify private static analyzer scratch space: {error}")
        })?;
        let canonical = path.canonicalize().map_err(|error| {
            format!("Cannot verify private static analyzer scratch space: {error}")
        })?;
        if !metadata.is_dir() || metadata.mode() & 0o7077 != 0 || canonical != path {
            return Err("Invalid private static analyzer scratch identity".into());
        }
        Ok(())
    }
}

fn prepare_review_authority(raw: &str) -> Result<ReviewAuthorityWireV1, String> {
    let wire: ReviewAuthorityWireV1 = serde_json::from_str(raw)
        .map_err(|error| format!("Invalid --review-authority: {error}"))?;
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    if wire.schema_version != 1 {
        return Err("--review-authority accepts only schema_version 1".into());
    }
    if !valid_id(&wire.checkpoint_id) || !valid_id(&wire.reviewer_id) {
        return Err("Invalid --review-authority identity".into());
    }
    if wire.findings.iter().any(|finding| {
        finding.disposition == polaris_core::workflow::ReviewFindingDispositionV1::Open
    }) {
        return Err("--review-authority does not accept open findings".into());
    }
    let validation_record = polaris_core::workflow::ReviewRecordV1 {
        schema_version: polaris_core::workflow::EVIDENCE_SCHEMA_VERSION,
        record_id: "review-authority-validation".into(),
        binding: polaris_core::workflow::EvidenceBindingV1 {
            task_id: "validation-task".into(),
            task_definition_hash: "0".repeat(64),
            subject_hash: "1".repeat(64),
            source_hash: "2".repeat(64),
            diff_hash: "3".repeat(64),
            artifact_graph_basis_hash: "4".repeat(64),
        },
        findings: wire.findings.clone(),
    };
    validation_record
        .validate()
        .map_err(|error| format!("Invalid --review-authority finding: {error}"))?;
    Ok(wire)
}

fn install_routine_authority(
    session: &mut Session,
    prompt: &str,
    prepared: PreparedRoutineAuthorityV1,
    project_root: &Path,
) -> Result<Arc<polaris_core::verification::SessionVerificationToolOwnerV1>, String> {
    let persistence = session
        .persistence
        .clone()
        .ok_or("--routine-authority requires a persistent session")?;
    let active_reference = session
        .workflow
        .as_ref()
        .and_then(|workflow| workflow.state().active_change_contract.clone());
    if active_reference.is_none() {
        let workflow = session
            .workflow
            .as_ref()
            .ok_or("--routine-authority requires an enabled workflow")?;
        if workflow.state().phase != Phase::Specify {
            workflow.request_phase(Phase::Specify)?;
            session.checkpoint().map_err(|error| error.to_string())?;
        }
    }
    let authorization = if let Some(reference) = active_reference {
        let contract = persistence
            .load_change_contract(&reference)
            .map_err(|error| format!("Cannot read the active change contract: {error}"))?;
        if contract.eligibility != prepared.eligibility
            || contract.verification != prepared.verification
        {
            return Err("The saved contract does not match --routine-authority".into());
        }
        None
    } else {
        Some(polaris_core::session::RoutineChangeAuthorizationV1::new(
            prompt,
            prepared.eligibility.clone(),
            prepared.verification.clone(),
        )?)
    };
    let cargo_program = resolve_cargo_program()?;
    let observer = Arc::new(CliVerificationObserverV1 {
        project_root: project_root.to_path_buf(),
        component_root: prepared.component_root,
        subject_hash: prepared.subject_hash,
        cargo_program,
    });
    let mut owner = polaris_core::verification::SessionVerificationToolOwnerV1::new(
        persistence,
        observer,
        prepared.runners,
    )?
    .with_preparation(prepared.verification_preparation)?;
    if let Some(executor) = prepared.existing_test_executor {
        owner = owner.with_existing_test_executor(Arc::new(executor))?;
    }
    owner = owner.with_static_adapters(prepared.static_adapters)?;
    let owner = Arc::new(owner);
    session.routine_change_authorization = authorization;
    session.verification_owner = Some(owner.clone());
    Ok(owner)
}

fn apply_review_authority(
    session: &mut Session,
    owner: &polaris_core::verification::SessionVerificationToolOwnerV1,
    authority: ReviewAuthorityWireV1,
) -> Result<(), String> {
    let (workflow, checkpoint) = owner.publish_host_review(
        authority.expected_workflow_revision,
        &authority.checkpoint_id,
        &authority.reviewer_id,
        authority.findings,
    )?;
    session.workflow = Some(workflow);
    session.verification_checkpoint = Some(checkpoint);
    Ok(())
}

fn resolve_cargo_program() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os("CARGO") {
        let configured = PathBuf::from(configured);
        if configured.is_absolute() {
            candidates.push(configured);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join("cargo")));
    }
    for candidate in candidates {
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        let Ok(metadata) = canonical.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(canonical);
    }
    Err("Cannot verify the pinned cargo executable for cargo metadata".into())
}

fn cargo_closure_hashes(
    project_root: &Path,
    component_root: &Path,
    cargo_program: &Path,
) -> Result<(String, String), String> {
    const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
    const MAX_LOCAL_PACKAGES: usize = 256;

    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("Cannot verify the workspace root: {error}"))?;
    let component_manifest = component_root
        .join("Cargo.toml")
        .canonicalize()
        .map_err(|error| format!("Cannot verify the component manifest: {error}"))?;
    let cargo_program = cargo_program
        .canonicalize()
        .map_err(|error| format!("Cannot verify the cargo executable: {error}"))?;
    let cargo_bytes = read_regular_resolution_input(&cargo_program, "cargo executable")?;
    let output = std::process::Command::new(&cargo_program)
        .current_dir(&project_root)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--manifest-path",
        ])
        .arg(project_root.join("Cargo.toml"))
        .output()
        .map_err(|error| format!("Cannot run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err("cargo metadata did not return a complete locked/offline closure".into());
    }
    if output.stdout.is_empty() || output.stdout.len() > MAX_METADATA_BYTES {
        return Err("cargo metadata output exceeds verification bounds".into());
    }
    let metadata: CargoMetadataBindingV1 =
        serde_json::from_slice(&output.stdout).map_err(|_| "Cannot parse cargo metadata output")?;
    let workspace_root = PathBuf::from(&metadata.workspace_root)
        .canonicalize()
        .map_err(|error| format!("Cannot verify the cargo workspace root: {error}"))?;
    if workspace_root != project_root {
        return Err("The cargo metadata workspace root does not match the project root".into());
    }
    let resolve = metadata
        .resolve
        .ok_or("cargo metadata has no complete resolve graph")?;
    let packages = metadata
        .packages
        .into_iter()
        .map(|package| (package.id.clone(), package))
        .collect::<std::collections::BTreeMap<_, _>>();
    let nodes = resolve
        .nodes
        .into_iter()
        .map(|node| (node.id.clone(), node.dependencies))
        .collect::<std::collections::BTreeMap<_, _>>();
    if packages.is_empty() || nodes.is_empty() || packages.keys().any(|id| !nodes.contains_key(id))
    {
        return Err("The cargo metadata package/resolve graph is incomplete".into());
    }
    let component_id = packages
        .values()
        .filter_map(|package| {
            let path = PathBuf::from(&package.manifest_path);
            (path.canonicalize().ok().as_ref() == Some(&component_manifest))
                .then_some(package.id.clone())
        })
        .collect::<Vec<_>>();
    let [component_id] = component_id.as_slice() else {
        return Err("Cannot uniquely resolve the component package in cargo metadata".into());
    };
    let mut pending = vec![component_id.clone()];
    let mut reachable = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        let dependencies = nodes
            .get(&id)
            .ok_or("Missing dependency node in cargo metadata")?;
        for dependency in dependencies {
            if !packages.contains_key(dependency) {
                return Err("Missing dependency package in cargo metadata".into());
            }
            pending.push(dependency.clone());
        }
    }
    let local = reachable
        .iter()
        .filter_map(|id| packages.get(id).filter(|package| package.source.is_none()))
        .collect::<Vec<_>>();
    if local.is_empty() || local.len() > MAX_LOCAL_PACKAGES {
        return Err("The local Cargo dependency closure exceeds verification bounds".into());
    }

    let mut resolution_inputs = Vec::new();
    resolution_inputs.push(hash_resolution_input(
        &cargo_program,
        &cargo_bytes,
        "cargo-program",
    ));
    let root_manifest = project_root.join("Cargo.toml");
    resolution_inputs.push(hash_file_resolution_input(
        &root_manifest,
        "workspace manifest",
    )?);
    let lockfile = project_root.join("Cargo.lock");
    resolution_inputs.push(hash_file_resolution_input(&lockfile, "Cargo.lock")?);
    for directory in cargo_config_search_directories(&project_root)? {
        resolution_inputs.push((
            "Cargo configuration search root".into(),
            directory.to_string_lossy().into_owned(),
            if directory.exists() {
                "present"
            } else {
                "absent"
            }
            .into(),
        ));
        for name in ["config", "config.toml"] {
            let candidate = directory.join(name);
            if candidate.exists() {
                let canonical = candidate
                    .canonicalize()
                    .map_err(|error| format!("Cannot verify Cargo configuration: {error}"))?;
                resolution_inputs.push(hash_file_resolution_input(
                    &canonical,
                    "Cargo configuration",
                )?);
            }
        }
    }
    resolution_inputs.sort();
    resolution_inputs.dedup();

    let mut local_sources = Vec::with_capacity(local.len());
    for package in local {
        let manifest = PathBuf::from(&package.manifest_path);
        let canonical_manifest = manifest
            .canonicalize()
            .map_err(|error| format!("Cannot verify the local dependency manifest: {error}"))?;
        if canonical_manifest != manifest
            || canonical_manifest
                .file_name()
                .and_then(|name| name.to_str())
                != Some("Cargo.toml")
        {
            return Err("Invalid local dependency manifest identity".into());
        }
        let root = canonical_manifest
            .parent()
            .ok_or("Missing local dependency root")?;
        local_sources.push((
            package.id.clone(),
            canonical_manifest.to_string_lossy().into_owned(),
            bounded_tree_hash(root)?,
        ));
    }
    local_sources.sort();
    let metadata_hash = polaris_core::conversation_state::content_hash(&output.stdout);
    let graph_bytes = serde_json::to_vec(&(
        "polaris.cargo-closure.graph.v1",
        &metadata_hash,
        &resolution_inputs,
        &reachable,
    ))
    .map_err(|_| "Cannot hash the Cargo dependency graph")?;
    let project_graph_hash = polaris_core::conversation_state::content_hash(&graph_bytes);
    let source_bytes = serde_json::to_vec(&(
        "polaris.cargo-closure.source.v1",
        &project_graph_hash,
        &local_sources,
    ))
    .map_err(|_| "Cannot hash Cargo dependency sources")?;
    Ok((
        polaris_core::conversation_state::content_hash(&source_bytes),
        project_graph_hash,
    ))
}

fn cargo_config_search_directories(project_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut directories = project_root
        .ancestors()
        .map(|directory| directory.join(".cargo"))
        .collect::<std::collections::BTreeSet<_>>();
    let cargo_home = if let Some(configured) = std::env::var_os("CARGO_HOME") {
        let configured = PathBuf::from(configured);
        if !configured.is_absolute() {
            return Err("CARGO_HOME must be an absolute path".into());
        }
        configured
    } else {
        let home = std::env::var_os("HOME").ok_or("Cannot resolve Cargo home")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("HOME must be an absolute path".into());
        }
        home.join(".cargo")
    };
    directories.insert(if cargo_home.exists() {
        cargo_home
            .canonicalize()
            .map_err(|error| format!("Cannot verify Cargo home: {error}"))?
    } else {
        cargo_home
    });
    Ok(directories.into_iter().collect())
}

fn read_regular_resolution_input(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("Cannot verify {label}: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!("{label} must be a regular file"));
    }
    std::fs::read(path).map_err(|error| format!("Cannot read {label}: {error}"))
}

fn hash_resolution_input(path: &Path, bytes: &[u8], kind: &str) -> (String, String, String) {
    (
        kind.into(),
        path.to_string_lossy().into_owned(),
        polaris_core::conversation_state::content_hash(bytes),
    )
}

fn hash_file_resolution_input(
    path: &Path,
    label: &str,
) -> Result<(String, String, String), String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Cannot verify {label}: {error}"))?;
    if canonical != path {
        return Err(format!("Aliases are not accepted for {label}"));
    }
    let bytes = read_regular_resolution_input(&canonical, label)?;
    Ok(hash_resolution_input(&canonical, &bytes, label))
}

fn bounded_tree_hash(root: &Path) -> Result<String, String> {
    const MAX_FILES: usize = 4096;
    const MAX_BYTES: u64 = 128 * 1024 * 1024;
    fn visit(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut entries = std::fs::read_dir(directory)
            .map_err(|error| format!("Cannot read the component tree: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Cannot read the component tree: {error}"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "target" | ".git" | ".polaris") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("Cannot verify the component tree: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "Cannot verify a symlink in the component tree: {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                visit(root, &path, files)?;
            } else if metadata.is_file() {
                files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
                if files.len() > MAX_FILES {
                    return Err(
                        "The component tree file count exceeds the verification limit".into(),
                    );
                }
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort();
    let mut total = 0_u64;
    let mut records = Vec::with_capacity(files.len());
    for relative in files {
        let bytes = std::fs::read(root.join(&relative))
            .map_err(|error| format!("Cannot read component source: {error}"))?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("component tree size overflow")?;
        if total > MAX_BYTES {
            return Err("The component tree exceeds verification bounds".into());
        }
        records.push((
            relative.to_string_lossy().into_owned(),
            polaris_core::conversation_state::content_hash(&bytes),
        ));
    }
    let encoded = serde_json::to_vec(&records).map_err(|_| "Cannot hash the component tree")?;
    Ok(polaris_core::conversation_state::content_hash(&encoded))
}

impl ToolMemoryArg {
    fn retention(self) -> Option<polaris_core::tool_memory::RetentionMode> {
        use polaris_core::tool_memory::RetentionMode;
        match self {
            Self::Off => None,
            Self::History => Some(RetentionMode::History),
            Self::Retrieval => Some(RetentionMode::Retrieval),
        }
    }
}

impl Args {
    fn validate_tool_memory(&self) -> Result<(), &'static str> {
        match (
            &self.tool_memory_embedding_url,
            &self.tool_memory_embedding_model,
        ) {
            (None, None) => Ok(()),
            (Some(url), Some(model))
                if self.tool_memory != ToolMemoryArg::Off
                    && !url.trim().is_empty()
                    && !model.trim().is_empty() =>
            {
                Ok(())
            }
            _ => Err(
                "Specify both a nonempty embedding URL and model, and enable --tool-memory history or retrieval",
            ),
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// Plan or provision a fixed development environment without model calls.
    Env(env::Args),
    /// Inspect typed decider candidates and pinned profiles, or explicitly install local resources.
    Decision(decision::Args),
    /// Register, reauthenticate, unlink, or label multiple accounts.
    Account(account::Args),
    /// Share across devices: join, inspect, send, receive, and leave.
    Sync(sync::Args),
    /// Search, retrieve, and delete locally stored conversation memory.
    Memory(memory::Args),
    /// List, enable, discover, and register MCP servers; manage credentials and verify catalogs.
    Mcp(mcp::Args),
    /// Detect Git and add missing ignore rules.
    Setting(setting::Args),
    /// Compare folder similarity using filenames and sizes only.
    Compare(compare::Args),
    /// Generate a request to verify the implementation on a real device.
    Handoff(handoff::Args),
    /// Assess whether spare capacity permits unrequested cleanup.
    Spare(spare::Args),
    /// Record a project decision as one statement. Before the next run, use it to check whether an already settled question is being reopened.
    Decided(decided::Args),
    /// Authenticate with a ChatGPT subscription. Opens a browser.
    Login,
    /// Delete the stored credentials. Does not touch `~/.codex/`.
    Logout,
    /// Run one instruction non-interactively. Equivalent to `--prompt`,
    /// offered as its own subcommand to match `codex exec`. Reads the
    /// instruction from stdin if omitted.
    Exec {
        /// The instruction to run. Read from stdin if omitted (or if `-`
        /// is given explicitly).
        prompt: Option<String>,
    },
    /// Run a command inside polaris's own sandbox policy, without going
    /// through the agent loop. Mirrors `codex sandbox` — useful for
    /// checking what a given `--sandbox` mode would allow.
    Sandbox {
        /// The program and its arguments to run under confinement.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Diagnose the local polaris installation: which credentials are
    /// saved, which provider would be picked with no `POLARIS_PROVIDER`
    /// set, and whether this platform's sandbox backend is available.
    Doctor,
    /// Run a shared black-box test plan inside the selected sandbox.
    CommonTests {
        /// The plan, as JSON. `-` reads it from stdin.
        plan: String,
    },
    /// Internal host-owned static check. It accepts no path or command.
    #[command(hide = true)]
    StaticComponentManifest,
    /// Generate a shell completion script and print it to stdout.
    Completion {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

/// The possible values for `--sandbox`. We can't make
/// `polaris_sandbox::SandboxMode` implement clap's `ValueEnum` directly
/// because both are types from other crates and that would hit the orphan
/// rule. We insert this one conversion step instead.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
enum SandboxModeArg {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl From<SandboxModeArg> for SandboxMode {
    fn from(a: SandboxModeArg) -> Self {
        match a {
            SandboxModeArg::ReadOnly => SandboxMode::ReadOnly,
            SandboxModeArg::WorkspaceWrite => SandboxMode::WorkspaceWrite,
            SandboxModeArg::FullAccess => SandboxMode::FullAccess,
        }
    }
}

/// The possible values for `--approval`. Same reason as `SandboxModeArg`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
enum ApprovalPolicyArg {
    Never,
    OnRequest,
    Always,
}

impl From<ApprovalPolicyArg> for ApprovalPolicy {
    fn from(a: ApprovalPolicyArg) -> Self {
        match a {
            ApprovalPolicyArg::Never => ApprovalPolicy::Never,
            ApprovalPolicyArg::OnRequest => ApprovalPolicy::OnRequest,
            ApprovalPolicyArg::Always => ApprovalPolicy::Always,
        }
    }
}

fn registry_revision(host: &polaris_core::mcp::local::LocalHost) -> u64 {
    host.store()
        .and_then(|store| store.load_registry().ok().flatten())
        .map_or(0, |registry| registry.revision)
}

/// An `Approver` that asks `y` / `n` at the terminal. The only place that
/// reads stdin.
struct TerminalApprover;

impl Approver for TerminalApprover {
    fn ask(&mut self, reason: &str) -> Decision {
        eprintln!("Approval required: {reason}");
        eprint!("Allow this? [y/N] ");
        // Even if the flush fails (e.g. no terminal), still attempt the read_line that follows.
        let _ = io::stderr().flush();

        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            // Deny if we can't read. Treat this the same as unattended, and never let it through.
            return Decision::Deny;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Decision::Allow,
            _ => Decision::Deny,
        }
    }
}

/// Bridges `polaris-auth` to `polaris-provider`'s `TokenSource`. Putting
/// this conversion here means `polaris-auth` doesn't need to depend on the
/// provider crate.
struct AuthTokens {
    issuer: String,
    store: PathBuf,
}

fn to_provider_error(e: polaris_auth::AuthError) -> polaris_provider::ProviderError {
    match e {
        polaris_auth::AuthError::NotLoggedIn => {
            polaris_provider::ProviderError::Auth("Not logged in. Run `polaris login`.".into())
        }
        other => polaris_provider::ProviderError::Auth(other.to_string()),
    }
}

/// Determines `reasoning.effort` from the `chatgpt_plan_type` claim in
/// `access_token`. Returns `None` when it can't be determined, deferring
/// to the server's default.
fn effort_for(access_token: &str) -> Option<String> {
    let plan = polaris_auth::token::plan_type_from_access_token(access_token);
    polaris_auth::effort_for_plan_type(plan.as_deref()).map(|s| s.to_string())
}

/// Resolve both providers identically; an account plan cannot replace an
/// explicit or default model.
fn selected_model(
    explicit: Option<String>,
    environment: Option<String>,
    interactive: bool,
) -> String {
    explicit.or(environment).unwrap_or_else(|| {
        if interactive {
            polaris_tui::render::RECOMMENDED_MODEL
        } else {
            polaris_provider::codex::DEFAULT_MODEL
        }
        .to_string()
    })
}

/// Interactive turns let Jev choose effort. One-shot runs keep the historical
/// xhigh default so benchmarks and automation remain fixed unless explicitly
/// overridden.
fn selected_effort(explicit: Option<&str>, has_prompt: bool) -> Option<&str> {
    explicit.or(has_prompt.then_some("xhigh"))
}

#[allow(clippy::too_many_arguments)]
async fn restore_continuation_workflow(
    resume: Option<&str>,
    fork: Option<&str>,
    session: &mut Session,
    saved: &polaris_core::session_store::PersistedSession,
    config: &polaris_core::workflow::WorkflowConfig,
    workflow_mode: Option<WorkflowModeArg>,
    skills: &[polaris_skills::Skill],
    history_backend_factory: Option<&polaris_tui::HistoryBackendFactory>,
) -> Result<(), String> {
    let kind = match (resume, fork) {
        (Some(_), None) => polaris_tui::ContinuationKind::Resume,
        (None, Some(_)) => polaris_tui::ContinuationKind::Fork,
        (None, None) => return Ok(()),
        _ => unreachable!("clap rejects mutually exclusive session flags"),
    };
    let continuation = polaris_tui::PreparedContinuation {
        kind,
        persistence: saved.clone(),
    };
    if workflow_mode.is_some() {
        polaris_tui::restore_prepared_continuation_for_managed_override(
            session,
            &continuation,
            config,
            skills,
            history_backend_factory,
        )
        .await
    } else {
        polaris_tui::restore_prepared_continuation(
            session,
            &continuation,
            config,
            false,
            skills,
            history_backend_factory,
        )
        .await
    }
}

enum ContinuationSource {
    V2(V2Target),
    Legacy(Vec<polaris_provider::Message>),
}

struct V2Target {
    id: String,
    generation: u64,
    raw_hash: String,
    raw_offset: u64,
    history_mode: polaris_core::conversation_state::HistoryMode,
}

struct ResolvedContinuation {
    kind: polaris_tui::ContinuationKind,
    source: ContinuationSource,
    history_mode: polaris_core::conversation_state::HistoryMode,
}

fn valid_legacy_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn read_v2_target(data_root: &Path, project_id: &str, id: &str) -> std::io::Result<V2Target> {
    let store =
        polaris_core::conversation_state::ConversationStore::open(data_root, project_id, id)?;
    let snapshot = store.lock()?.snapshot()?;
    Ok(V2Target {
        id: snapshot.state.session_id,
        generation: snapshot.state.generation,
        raw_hash: snapshot.raw_hash,
        raw_offset: snapshot.raw_offset,
        history_mode: snapshot.state.history_mode,
    })
}

fn v2_entry_is_absent(data_root: &Path, id: &str) -> std::io::Result<bool> {
    let root = data_root.join("sessions-v2");
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(std::io::Error::other("v2 session root is a symlink"))
        }
        Ok(_) => match std::fs::symlink_metadata(root.join(id)) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Computes the normal storage locations without creating their directories.
/// Resume/fork validation must happen before onboarding or helper staging can
/// leave unrelated files behind on a rejected target.
fn preflight_storage_paths(cwd: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let data_root =
        match std::env::var_os("POLARIS_DATA_DIR") {
            Some(value) if value.is_empty() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "POLARIS_DATA_DIR must not be empty",
                ));
            }
            Some(value) => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "POLARIS_DATA_DIR must be an absolute path",
                    ));
                }
                path
            }
            None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set")
            })?)
            .join(".polaris"),
        };
    let project = polaris_core::project::resolve_root(cwd);
    Ok((
        data_root
            .join("state")
            .join(polaris_core::project::project_id(&project)),
        data_root.join("sessions"),
    ))
}

/// Resolve a continuation before provider onboarding or helper staging. Only
/// an absent v2 session can take the explicit legacy JSONL import path.
fn resolve_continuation(
    resume: Option<&str>,
    fork: Option<&str>,
    data_root: &Path,
    project_id: &str,
    requested_history_mode: Option<polaris_core::conversation_state::HistoryMode>,
    default_history_mode: polaris_core::conversation_state::HistoryMode,
    legacy_sessions_dir: &Path,
) -> std::io::Result<Option<ResolvedContinuation>> {
    let continuation = match (resume, fork) {
        (Some(id), None) => {
            if valid_legacy_session_id(id) && v2_entry_is_absent(data_root, id)? {
                let legacy = legacy_sessions_dir.join(format!("{id}.jsonl"));
                let metadata = std::fs::symlink_metadata(&legacy)?;
                if !metadata.file_type().is_file() {
                    return Err(std::io::Error::other(
                        "legacy session is not a regular file",
                    ));
                }
                let legacy_session = polaris_tui::persist::read_session_strict(&legacy)?;
                return Ok(Some(ResolvedContinuation {
                    kind: polaris_tui::ContinuationKind::Resume,
                    source: ContinuationSource::Legacy(legacy_session.messages),
                    history_mode: default_history_mode,
                }));
            }
            let target = read_v2_target(data_root, project_id, id)?;
            let saved_mode = target.history_mode;
            if requested_history_mode.is_some_and(|mode| mode != saved_mode) {
                return Err(std::io::Error::other(
                    "--history-mode does not match the saved session mode",
                ));
            }
            ResolvedContinuation {
                kind: polaris_tui::ContinuationKind::Resume,
                source: ContinuationSource::V2(target),
                history_mode: saved_mode,
            }
        }
        (None, Some(id)) => {
            let target = read_v2_target(data_root, project_id, id)?;
            let saved_mode = target.history_mode;
            if requested_history_mode.is_some_and(|mode| mode != saved_mode) {
                return Err(std::io::Error::other(
                    "--history-mode does not match the saved session mode",
                ));
            }
            ResolvedContinuation {
                kind: polaris_tui::ContinuationKind::Fork,
                source: ContinuationSource::V2(target),
                history_mode: saved_mode,
            }
        }
        (None, None) => return Ok(None),
        _ => unreachable!("clap rejects mutually exclusive session flags"),
    };
    Ok(Some(continuation))
}

#[cfg(unix)]
fn resolve_pending_workspace_before_execution(
    project_root: &Path,
    state_dir: &Path,
) -> std::io::Result<()> {
    let store_root = state_dir.join("rewind-workspace");
    match std::fs::symlink_metadata(&store_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let owner = polaris_core::rewind_workspace::WorkspaceOwner::open(
        project_root,
        &store_root,
        polaris_core::rewind_workspace::RewindLimits::default(),
    )
    .map_err(std::io::Error::other)?;
    match owner.resolve_pending().map_err(std::io::Error::other)? {
        polaris_core::rewind_workspace::ResolveOutcome::Clean => Ok(()),
        polaris_core::rewind_workspace::ResolveOutcome::RolledBack(_) => {
            Err(std::io::Error::other(
                "Recovered files from an interrupted rewind. Restart before resuming the conversation",
            ))
        }
    }
}

#[cfg(unix)]
fn validate_resolved_continuation_workspace(
    resolved: &ResolvedContinuation,
    project_root: &Path,
    state_dir: &Path,
    data_root: &Path,
    project_id: &str,
) -> std::io::Result<()> {
    let ContinuationSource::V2(target) = &resolved.source else {
        return Ok(());
    };
    let owner = polaris_core::rewind_workspace::WorkspaceOwner::open(
        project_root,
        &state_dir.join("rewind-workspace"),
        polaris_core::rewind_workspace::RewindLimits::default(),
    )
    .map_err(std::io::Error::other)?;
    let persistence = polaris_core::session_store::PersistedSession::open(
        data_root,
        &state_dir.join("memory.sqlite3"),
        project_id,
        &target.id,
    )?;
    polaris_core::rewind_admission::validate_execution(&owner, &persistence)
}

/// Fork/import only after provider, helper, and strict-history preconditions
/// have succeeded. Resume merely carries its earlier validated attachment.
#[cfg(any(test, not(unix)))]
fn prepare_continuation(
    resolved: ResolvedContinuation,
    data_root: &Path,
    database: &Path,
    project_id: &str,
) -> std::io::Result<polaris_tui::PreparedContinuation> {
    prepare_continuation_with(resolved, data_root, database, project_id, |source| {
        source.fork("")
    })
}

fn prepare_continuation_with(
    resolved: ResolvedContinuation,
    data_root: &Path,
    database: &Path,
    project_id: &str,
    fork: impl FnOnce(
        &polaris_core::session_store::PersistedSession,
    ) -> std::io::Result<polaris_core::session_store::PersistedSession>,
) -> std::io::Result<polaris_tui::PreparedContinuation> {
    let persistence = match resolved.source {
        ContinuationSource::V2(target) => {
            let persistence = polaris_core::session_store::PersistedSession::open(
                data_root, database, project_id, &target.id,
            )?;
            let snapshot = persistence.snapshot()?;
            if snapshot.state.session_id != target.id
                || snapshot.state.generation != target.generation
                || snapshot.state.history_mode != target.history_mode
                || snapshot.raw_hash != target.raw_hash
                || snapshot.raw_offset != target.raw_offset
            {
                return Err(std::io::Error::other(
                    "The v2 session changed after verification. Resume it again",
                ));
            }
            match resolved.kind {
                polaris_tui::ContinuationKind::Resume => persistence,
                polaris_tui::ContinuationKind::Fork => fork(&persistence)?,
            }
        }
        ContinuationSource::Legacy(messages) => {
            polaris_core::session_store::PersistedSession::import(
                data_root,
                database,
                project_id,
                resolved.history_mode,
                None,
                &messages,
            )?
        }
    };
    Ok(polaris_tui::PreparedContinuation {
        kind: resolved.kind,
        persistence,
    })
}

#[cfg(unix)]
fn with_cli_management_lease<T>(
    cwd: &Path,
    state_dir: &Path,
    data_root: &Path,
    project_id: &str,
    action: impl FnOnce(
        &polaris_core::project_mutation::ProjectMutationLeaseV1,
        &polaris_core::management_effect::ManagementEffectStoreV1,
        &polaris_core::rewind_workspace::WorkspaceOwner,
    ) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let owner = polaris_core::project_mutation::ProjectMutationOwnerV1::for_project(cwd)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let lease = owner
        .try_mutation()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let rewind = polaris_core::rewind_workspace::WorkspaceOwner::open(
        &polaris_core::project::resolve_root(cwd),
        &state_dir.join("rewind-workspace"),
        polaris_core::rewind_workspace::RewindLimits::default(),
    )
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    rewind
        .require_quiescent_with_lease(&lease)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let project_root = polaris_core::project::resolve_root(cwd);
    let effects = polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
        &project_root,
        state_dir,
        &lease,
    )
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    polaris_core::session_switch::SessionSwitchOwner {
        effects: &effects,
        lease: &lease,
        state_dir,
    }
    .recover(data_root, &state_dir.join("memory.sqlite3"), project_id)
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    polaris_core::management_effect::workflow_change::WorkflowChangeOwner {
        effects: &effects,
        lease: &lease,
        state_dir,
    }
    .recover(data_root, &state_dir.join("memory.sqlite3"), project_id)
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Some(effect) =
        polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_mutation(
            &project_root,
            state_dir,
            &lease,
        )
        .map_err(|error| std::io::Error::other(error.to_string()))?
    {
        return Err(std::io::Error::other(format!(
            "Unresolved management operation {} ({})",
            effect.request.operation_id, effect.request.inventory
        )));
    }
    action(&lease, &effects, &rewind)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn apply_cli_workflow_override(
    session: &mut Session,
    mode: WorkflowModeArg,
    skills: &[polaris_skills::Skill],
    cwd: &Path,
    state_dir: &Path,
    data_root: &Path,
    project_id: &str,
) -> Result<(), String> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    apply_cli_workflow_override_with_confirmation(
        session,
        mode,
        skills,
        cwd,
        state_dir,
        data_root,
        project_id,
        stdin.is_terminal(),
        stderr.is_terminal(),
        &mut stdin.lock(),
        &mut stderr.lock(),
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn apply_cli_workflow_override_with_confirmation<R: BufRead, W: Write>(
    session: &mut Session,
    mode: WorkflowModeArg,
    skills: &[polaris_skills::Skill],
    cwd: &Path,
    state_dir: &Path,
    data_root: &Path,
    project_id: &str,
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
    input: &mut R,
    output: &mut W,
) -> Result<(), String> {
    use polaris_core::management_effect::workflow_change::{
        WorkflowChangeError, WorkflowChangeOwner, WorkflowEffectInventoryV1,
    };

    session
        .check_persistence()
        .map_err(|error| error.to_string())?;
    let current = session
        .workflow
        .as_ref()
        .ok_or("No saved workflow configuration")?;
    let mut config = current.config.clone();
    mode.apply(&mut config);
    let candidate = polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
        config,
        current.gates.clone(),
        current.state(),
        current.artifact_graph().clone(),
    )?;
    if candidate.config.enabled {
        drop(candidate.begin_turn_from_disk(skills)?);
    }
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("No saved conversation")?;
    let preview =
        match WorkflowChangeOwner::preview(persistence, WorkflowEffectInventoryV1::F15, &candidate)
        {
            Ok(preview) => preview,
            Err(WorkflowChangeError::NoChange) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
    if !stdin_is_terminal || !stderr_is_terminal {
        return Err("Changing --workflow-mode for an existing conversation requires confirmation in an interactive terminal".into());
    }
    let before = preview
        .before_config()
        .ok_or("Missing workflow configuration before the change")?;
    let after = preview
        .candidate_config()
        .ok_or("Missing workflow configuration after the change")?;
    writeln!(
        output,
        "F15 workflow change: project={} session={} generation={} rewind=CapturedRestorable / phase {} → {} / mode {} → {}",
        preview.project_id(),
        preview.session_id(),
        preview.before_generation(),
        preview.before_workflow().phase,
        preview.candidate_workflow().phase,
        cli_workflow_mode_name(&before.config),
        cli_workflow_mode_name(&after.config),
    )
    .map_err(|error| error.to_string())?;
    write!(output, "Accept this saved change? [y/N] ").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    if answer.trim() != "y" {
        return Err("Workflow change cancelled".into());
    }
    with_cli_management_lease(
        cwd,
        state_dir,
        data_root,
        project_id,
        |lease, effects, _rewind| {
            let owner = WorkflowChangeOwner {
                effects,
                lease,
                state_dir,
            };
            let operation_id = polaris_core::management_effect::ManagementOperationIdV1::random()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let prepared = owner
                .prepare_confirmed(persistence, &preview, operation_id)
                .map_err(|error| io::Error::other(error.to_string()))?;
            owner
                .execute(persistence, &prepared)
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(())
        },
    )
    .map_err(|error| error.to_string())?;
    session.workflow = Some(candidate);
    Ok(())
}

#[cfg(unix)]
fn cli_workflow_mode_name(config: &polaris_core::workflow::WorkflowConfig) -> &'static str {
    if !config.enabled {
        "off"
    } else {
        match config.mode {
            polaris_core::workflow::WorkflowMode::Manual => "manual",
            polaris_core::workflow::WorkflowMode::Adaptive => "adaptive",
        }
    }
}

#[cfg(target_os = "macos")]
fn strict_history(
    config: &polaris_core::config::EmbeddingConfig,
    runtime_home: &Path,
    provider_name: &str,
    private_root: &Path,
    attempt_ledger: polaris_provider::attempts::AttemptLedger,
    parent_id: &str,
    decider: Option<std::sync::Arc<dyn polaris_provider::decision::Decider>>,
) -> Result<
    (
        std::sync::Arc<polaris_core::conversation_memory::StrictHistory>,
        polaris_provider::UsageMeter,
    ),
    String,
> {
    let provider: std::sync::Arc<dyn polaris_provider::Provider> = match provider_name {
        "openai" => {
            let base = std::env::var("POLARIS_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into());
            let key = std::env::var("POLARIS_API_KEY")
                .ok()
                .or_else(|| {
                    polaris_auth::api_key::default_path()
                        .ok()
                        .and_then(|path| polaris_auth::api_key::load_from(&path).ok().flatten())
                })
                .ok_or("POLARIS_API_KEY is not set")?;
            std::sync::Arc::new(
                OpenAiProvider::new(base, key, "gpt-6-astra".into())
                    .map_err(|e| e.to_string())?
                    .with_attempt_ledger(attempt_ledger.scoped(
                        polaris_provider::attempts::AttemptContext {
                            parent_id: Some(parent_id.to_string()),
                            kind: polaris_provider::attempts::AttemptKind::Summary,
                        },
                    )),
            )
        }
        "codex" => {
            let store = polaris_auth::store::default_path().map_err(|e| e.to_string())?;
            // 主モデルと同じ登録を使う。要約だけ別のアカウントへ送らない。
            let tokens = crate::account::require(store)?;
            std::sync::Arc::new(
                polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.into(),
                    "gpt-6-astra".into(),
                    tokens,
                )
                .map_err(|e| e.to_string())?
                .with_attempt_ledger(attempt_ledger.scoped(
                    polaris_provider::attempts::AttemptContext {
                        parent_id: Some(parent_id.to_string()),
                        kind: polaris_provider::attempts::AttemptKind::Summary,
                    },
                )),
            )
        }
        other => return Err(format!("unknown provider {other}")),
    };
    provider.set_effort(Some("medium"));
    let summary = std::sync::Arc::new(polaris_core::strict_provider::ProviderSummary::new(
        provider,
    ));
    let usage = summary.usage.clone();
    #[cfg(target_os = "macos")]
    let mut embedder = {
        let runtime = std::sync::Arc::new(
            polaris_core::managed_runtime::ManagedRuntimeLease::acquire(runtime_home).map_err(
                |error| format!("Read-only runtime image unavailable; repair required: {error}"),
            )?,
        );
        polaris_core::strict_provider::managed_local_embedder(config, private_root, runtime)
            .map_err(|error| error.to_string())?
    };
    #[cfg(not(target_os = "macos"))]
    let mut embedder = {
        let _ = runtime_home;
        return Err(
            "The strict10 read-only runtime image is unavailable in this environment".into(),
        );
    };
    embedder.attempt_ledger = Some(attempt_ledger);
    Ok((
        build_strict_history(summary, std::sync::Arc::new(embedder), decider),
        usage,
    ))
}

#[cfg(not(target_os = "macos"))]
fn strict_history(
    _config: &polaris_core::config::EmbeddingConfig,
    _runtime_home: &Path,
    _provider_name: &str,
    _private_root: &Path,
    _attempt_ledger: polaris_provider::attempts::AttemptLedger,
    _parent_id: &str,
    _decider: Option<std::sync::Arc<dyn polaris_provider::decision::Decider>>,
) -> Result<
    (
        std::sync::Arc<polaris_core::conversation_memory::StrictHistory>,
        polaris_provider::UsageMeter,
    ),
    String,
> {
    Err("The strict10 read-only runtime image is unavailable in this environment".into())
}

#[cfg(target_os = "macos")]
fn managed_runtime_home() -> Result<PathBuf, String> {
    polaris_core::os_home::trusted_user_home().map_err(|error| {
        format!("Cannot determine the OS user home for the managed runtime: {error}")
    })
}

#[cfg(any(test, not(target_os = "linux")))]
fn build_strict_history(
    summary: std::sync::Arc<dyn polaris_core::conversation_memory::StrictSummaryProvider>,
    embedder: std::sync::Arc<dyn polaris_core::conversation_memory::StrictEmbedder>,
    decider: Option<std::sync::Arc<dyn polaris_provider::decision::Decider>>,
) -> std::sync::Arc<polaris_core::conversation_memory::StrictHistory> {
    std::sync::Arc::new(
        polaris_core::conversation_memory::StrictHistory::new(summary, embedder).judged_by(decider),
    )
}

#[async_trait::async_trait]
impl polaris_provider::TokenSource for AuthTokens {
    async fn token(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let c = polaris_auth::ensure_fresh(&self.issuer, &self.store)
            .await
            .map_err(to_provider_error)?;
        let effort = effort_for(&c.access_token);
        Ok(polaris_provider::Token {
            access_token: c.access_token,
            account_id: c.account_id,
            effort,
        })
    }

    async fn refreshed(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let c = polaris_auth::force_refresh(&self.issuer, &self.store)
            .await
            .map_err(to_provider_error)?;
        let effort = effort_for(&c.access_token);
        Ok(polaris_provider::Token {
            access_token: c.access_token,
            account_id: c.account_id,
            effort,
        })
    }
}

/// Parses `--mcp-prompt` and its arguments into one explicit choice.
///
/// Values are taken as strings, not parsed as JSON: a prompt argument that
/// happens to look like a number or an object would otherwise change type
/// depending on what the user typed, and the server is told what it was
/// given rather than what it looked like.
fn mcp_prompt_choice(
    spec: &str,
    arguments: &[String],
) -> Result<polaris_core::mcp::runtime::McpPromptChoice, String> {
    let (server, prompt) = spec
        .split_once('/')
        .ok_or_else(|| format!("Specify --mcp-prompt as server/prompt: {spec}"))?;
    if server.is_empty() || prompt.is_empty() {
        return Err(format!(
            "Missing server or prompt name in --mcp-prompt: {spec}"
        ));
    }
    let mut map = serde_json::Map::new();
    for argument in arguments {
        let (key, value) = argument
            .split_once('=')
            .ok_or_else(|| format!("Specify --mcp-prompt-arg as key=value: {argument}"))?;
        if key.is_empty() {
            return Err(format!("Missing key in --mcp-prompt-arg: {argument}"));
        }
        if map
            .insert(key.to_owned(), serde_json::Value::String(value.to_owned()))
            .is_some()
        {
            return Err(format!("Duplicate --mcp-prompt-arg key: {key}"));
        }
    }
    Ok(polaris_core::mcp::runtime::McpPromptChoice {
        server: server.to_owned(),
        prompt: prompt.to_owned(),
        arguments: map,
    })
}

/// The typed judge, built once and shared by everything that consults it.
///
/// Returns `None` both when it is switched off and when it is on but
/// unusable, because neither is a reason to stop: every path that asks it
/// something works without an answer. An unusable one says so on stderr —
/// silently running without a judge the user asked for would look like the
/// judge simply never helped.
/// Trims this project's build output to its cap, after the run and not
/// during it.
///
/// **Timing is the safety here.** A build that is running holds files this
/// would otherwise consider, and the run that just ended is the moment when
/// the fewest of them are held. The age and link rules in
/// `polaris_core::build_cache` still apply on top of that; this only picks a
/// moment when they have the least work to do.
///
/// Nothing is said unless something was removed or the cap could not be met.
/// A sweep that found nothing to do is not news.
fn sweep_build_cache(cwd: &std::path::Path, config: &polaris_core::config::Config) {
    use polaris_core::build_cache;
    let Some(directory) = build_cache::directory(cwd, config.build_cache_path.as_deref()) else {
        return;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    // A share of an unknown total is not a cap; the absolute one decides alone.
    let total = build_cache::filesystem_bytes(&directory);
    let Ok(swept) = build_cache::sweep(&directory, config.build_cache, total, now.as_secs()) else {
        return;
    };
    if swept.removed > 0 {
        eprintln!(
            "Cleaned up {} build artifacts ({:.1} GB): {}",
            swept.removed,
            swept.freed_bytes as f64 / 1e9,
            directory.display()
        );
    }
    if swept.still_over() {
        // Saying nothing here would leave the person believing the cap holds.
        eprintln!(
            "Build artifacts still exceed the limit: {}. Remaining artifacts are recent or referenced elsewhere.",
            directory.display()
        );
    }
    if swept.skipped > 0 {
        eprintln!(
            "Skipped {} items during the final pre-deletion check. Another build may have modified them.",
            swept.skipped
        );
    }
}

/// The judge this run will use, or none with the reason said out loud.
///
/// **A backend that cannot start never becomes the other one.** The
/// adoption plan is explicit that a missing helper, a hash mismatch or a
/// failed preflight falls back to the path that works without a judge at
/// all, not to the remote one: a run that quietly answers with a
/// different decider than the configuration names produces evidence
/// nobody can attribute. Every return prints the effective backend, so
/// "which judge answered" is never inferred from the absence of a
/// message.
fn build_decider(
    config: &polaris_core::config::DecisionConfig,
) -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
    use polaris_core::decision_bands::{DecisionUse, build_configured};

    match build_configured(config, DecisionUse::Routing) {
        Ok(Some(decider)) => {
            eprintln!("[decision] backend = \"{}\"", decider.name());
            Some(decider)
        }
        Ok(None) => None,
        Err(reason) => {
            eprintln!(
                "[decision] cannot use backend = \"{}\" ({reason}). Continuing without a decider",
                config.backend.as_str()
            );
            None
        }
    }
}

/// Final-answer observation has a separate one-shot disclosure boundary. The
/// normal Jev client may retry transient failures, so it cannot be reused for
/// this path.
fn build_finalizer_decider(
    config: &polaris_core::config::DecisionConfig,
) -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
    polaris_core::decision_bands::build_configured(
        config,
        polaris_core::decision_bands::DecisionUse::Finalization,
    )
    .ok()
    .flatten()
}

#[cfg(unix)]
async fn build_tui_answer_finalizer(
    config: &polaris_core::config::Config,
    state_dir: &Path,
    managed_home: Option<&Path>,
) -> Result<
    Option<std::sync::Arc<polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>>,
    String,
> {
    use polaris_core::config::{AnswerFinalizerMode, PresentationLanguage, TranslationProfile};

    let decider = build_finalizer_decider(&config.decision);
    let translation_requested = config
        .answer_finalizer
        .translation
        .activation
        .is_requested();
    let mut installed = None;
    let (translator, unavailable) =
        if config.answer_finalizer.mode != AnswerFinalizerMode::Shadow || !translation_requested {
            (None, None)
        } else if config.presentation.language != PresentationLanguage::Ja {
            (
            None,
            Some(
                polaris_core::answer_finalizer::shadow::ShadowFailureV1::PresentationLanguageNotJa,
            ),
        )
        } else if config.answer_finalizer.translation.profile
            != Some(TranslationProfile::CatTranslate3_3b)
        {
            (
                None,
                Some(polaris_core::answer_finalizer::shadow::ShadowFailureV1::ModelUnavailable),
            )
        } else {
            let resolved_home;
            let home = if let Some(home) = managed_home {
                home
            } else {
                resolved_home = polaris_core::os_home::trusted_user_home().map_err(|error| {
                    format!("Cannot determine the OS user home for the managed model: {error}")
                })?;
                &resolved_home
            };
            match polaris_core::translation::ManagedCatTranslate::open_with_installation(
                home,
                &mut installed,
            ) {
                Ok(translator) if translator.preflight().await.is_ok() => (
                    Some(std::sync::Arc::new(translator)
                        as std::sync::Arc<
                            dyn polaris_core::translation::TranslationBackendV1,
                        >),
                    None,
                ),
                _ => (
                    None,
                    Some(polaris_core::answer_finalizer::shadow::ShadowFailureV1::ModelUnavailable),
                ),
            }
        };
    let mut owner = polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner::open_configured(
        state_dir.join("answer-finalizer-shadow-v1.json"),
        config.presentation.clone(),
        config.answer_finalizer.clone(),
        decider,
        translator,
        unavailable,
    )
    .map_err(|error| error.to_string())?;
    if let Some(owner) = owner.as_mut() {
        std::sync::Arc::get_mut(owner)
            .ok_or("finalizer startup observation is already shared")?
            .record_translation_installation(installed);
    }
    Ok(owner)
}

#[cfg(not(unix))]
async fn build_tui_answer_finalizer(
    _config: &polaris_core::config::Config,
    _state_dir: &Path,
    _managed_home: Option<&Path>,
) -> Result<
    Option<std::sync::Arc<polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>>,
    String,
> {
    Err("Answer post-processing is not yet available on Windows".into())
}

/// The provider handle everything downstream holds. Named so the three
/// that come out of the construction below read as what they are.
type Shared = std::sync::Arc<dyn polaris_provider::Provider>;

/// One entry per priced weight, keyed by the weight's own spelling.
type TierWeights = std::collections::HashMap<String, Shared>;

/// Builds one provider instance per priced weight. Each gets its own model
/// and effort at construction and is never retuned afterwards, because the
/// instances are shared across a wave of concurrent delegations — switching
/// a model on one of them would change it for every task holding the same
/// `Arc`. A weight that prices nothing is skipped and keeps the parent.
///
/// `build` is per backend; it returns the error text rather than a provider
/// so that a weight naming something the backend will not accept stops the
/// run here instead of at the first delegation.
fn tier_providers(
    tiers: &polaris_core::config::TierConfig,
    parent_model: &str,
    session_effort: Option<&str>,
    mut build: impl FnMut(&str) -> Result<std::sync::Arc<dyn polaris_provider::Provider>, String>,
) -> Result<std::collections::HashMap<String, std::sync::Arc<dyn polaris_provider::Provider>>, String>
{
    let mut held = std::collections::HashMap::new();
    for tier in polaris_skills::Tier::ALL {
        let settings = tiers.for_tier(tier);
        if settings.model.is_none() && settings.effort.is_none() {
            continue;
        }
        // Pricing only an effort is a real choice: the same model, told to
        // think harder for the heavy weight.
        let provider = build(settings.model.as_deref().unwrap_or(parent_model))
            .map_err(|error| format!("Cannot create the provider for [tier.{tier}]: {error}"))?;
        // The session's own `--effort` still applies to a weight that does
        // not name one, so the flag keeps meaning what it says.
        if let Some(effort) = settings.effort.as_deref().or(session_effort) {
            provider.set_effort(Some(effort));
        }
        held.insert(tier.name().to_owned(), provider);
    }
    Ok(held)
}

#[tokio::main]
async fn main() -> ExitCode {
    // Retain admission through every command, TUI run and confined helper.
    // The OS home and actual executable are never derived from argv or HOME.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let _entry_lease = match production_entry_lease() {
        Ok(lease) => lease,
        Err(()) => {
            eprintln!("Cannot verify MCP migration state or the executable. Nothing was started.");
            return ExitCode::FAILURE;
        }
    };
    let args = Args::parse();
    // Only an argument-free Windows launch enters browsing. Explicit options,
    // even when equal to their defaults, retain the normal refusal path.
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_none() {
        return polaris_tui::run_windows_read_only();
    }
    #[cfg(target_os = "linux")]
    if linux_interactive_requested(&args) {
        return run_with_linux_signal_owner(
            || polaris_tui::signal::install_linux_process_signal_owner().map_err(|e| e.to_string()),
            || main_with_args(args),
            |diagnostic| eprintln!("{diagnostic}"),
        )
        .await;
    }
    main_with_args(args).await
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn production_entry_lease() -> Result<polaris_mcp::entry_gate::ProductionEntryLeaseV2, ()> {
    let home = polaris_core::os_home::trusted_user_home().map_err(|_| ())?;
    let executable = std::env::current_exe().map_err(|_| ())?;
    polaris_mcp::entry_gate::ProductionEntryLeaseV2::enter(&home, &executable).map_err(|_| ())
}

#[cfg(target_os = "linux")]
fn linux_interactive_requested(args: &Args) -> bool {
    args.prompt.is_none() && args.command.is_none() && !args.confined_read && !args.confined_apply
}

#[cfg(target_os = "linux")]
fn bounded_linux_signal_diagnostic(kind: &str, error: &str) -> String {
    let detail = error
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect::<String>();
    format!("Linux signal owner {kind} failed: {detail}")
}

#[cfg(target_os = "linux")]
async fn run_with_linux_signal_owner<Run, Fut>(
    install: impl FnOnce() -> Result<polaris_tui::signal::LinuxProcessSignalOwner, String>,
    run: Run,
    mut report: impl FnMut(&str),
) -> ExitCode
where
    Run: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ExitCode>,
{
    let mut owner = match install() {
        Ok(owner) => owner,
        Err(error) => {
            report(&bounded_linux_signal_diagnostic("installation", &error));
            return ExitCode::FAILURE;
        }
    };
    let result = run().await;
    let signal = match owner.try_uninstall() {
        Ok(signal) => signal,
        Err(error) => {
            report(&bounded_linux_signal_diagnostic(
                "restoration",
                &error.to_string(),
            ));
            return ExitCode::FAILURE;
        }
    };
    if result != ExitCode::SUCCESS {
        return result;
    }
    match signal {
        Some(polaris_tui::signal::LinuxProcessShutdown::SigInt) => ExitCode::from(130),
        Some(polaris_tui::signal::LinuxProcessShutdown::SigTerm) => ExitCode::from(143),
        None => result,
    }
}

async fn main_with_args(mut args: Args) -> ExitCode {
    if cfg!(windows)
        && matches!(
            args.command.as_ref(),
            None | Some(
                Command::Exec { .. } | Command::Sandbox { .. } | Command::CommonTests { .. }
            )
        )
    {
        eprintln!(
            "Windows lacks the required execution and mutation protections. No changes were made."
        );
        return ExitCode::FAILURE;
    }
    if args.web_search != WebSearchArg::Disabled {
        eprintln!(
            "Web search is unavailable because the endpoint capability contract is unverified. Use disabled."
        );
        return ExitCode::FAILURE;
    }
    if let Err(error) = args.validate_tool_memory() {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    #[cfg(not(windows))]
    let tool_memory_embedding = args
        .tool_memory_embedding_url
        .clone()
        .zip(args.tool_memory_embedding_model.clone());

    match args.command.take() {
        Some(Command::Env(options)) => {
            return env::run(options, args.approval.into(), args.sandbox.into()).await;
        }
        Some(Command::Account(options)) => return account::run(options).await,
        Some(Command::Sync(options)) => return sync::run(options).await,
        Some(Command::Memory(options)) => return memory::run(options).await,
        Some(Command::Mcp(options)) => return mcp::run(options).await,
        Some(Command::Decided(options)) => {
            return match decided::run(options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(reason) => {
                    eprintln!("{reason}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Command::Setting(options)) => return setting::run(options),
        Some(Command::Compare(options)) => return compare::run(options),
        Some(Command::Handoff(options)) => return handoff::run(options),
        Some(Command::Spare(options)) => return spare::run(options),
        Some(Command::Login) => {
            let store = match polaris_auth::store::default_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Can't determine where to store credentials: {e}");
                    return ExitCode::FAILURE;
                }
            };
            return match polaris_auth::login::run(polaris_auth::ISSUER, &store).await {
                Ok(_) => {
                    println!("Logged in: {}", store.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("Can't log in: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Command::Logout) => {
            let store = match polaris_auth::store::default_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Can't determine where to store credentials: {e}");
                    return ExitCode::FAILURE;
                }
            };
            return match polaris_auth::logout(&store) {
                Ok(true) => {
                    println!("Logged out: {}", store.display());
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    println!("Not logged in");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("Can't log out: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Command::Exec { prompt }) => {
            // Falls through to the same one-shot path `--prompt` already
            // takes below, rather than duplicating provider resolution,
            // sandbox setup, and the agent loop. `-` explicitly requests
            // stdin, matching `codex exec`'s own convention.
            args.prompt = Some(match prompt {
                Some(p) if p != "-" => p,
                _ => {
                    let mut buf = String::new();
                    if let Err(e) = io::stdin().read_to_string(&mut buf) {
                        eprintln!("Can't read the instruction from stdin: {e}");
                        return ExitCode::FAILURE;
                    }
                    buf
                }
            });
        }
        Some(Command::Sandbox { command }) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let policy = match build_sandbox_policy(&cwd, args.sandbox.into()) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Can't build the sandbox policy: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let program = Path::new(&command[0]);
            return match polaris_sandbox::run_confined(&policy, program, &command[1..], None) {
                Ok(outcome) => {
                    print!("{}", outcome.stdout);
                    eprint!("{}", outcome.stderr);
                    // A shell exit status is a small non-negative int in
                    // practice; ExitCode::from wants a u8, so anything
                    // outside that range collapses to FAILURE rather than
                    // silently wrapping.
                    match u8::try_from(outcome.status) {
                        Ok(code) => ExitCode::from(code),
                        Err(_) => ExitCode::FAILURE,
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Command::Doctor) => return run_doctor().await,
        Some(Command::Decision(args)) => return decision::run(args).await,
        Some(Command::CommonTests { plan }) => {
            return run_common_tests(&plan, args.sandbox.into());
        }
        Some(Command::StaticComponentManifest) => return run_static_component_manifest(),
        Some(Command::Completion { shell }) => {
            clap_complete::generate(
                shell,
                &mut <Args as clap::CommandFactory>::command(),
                "polaris",
                &mut io::stdout(),
            );
            return ExitCode::SUCCESS;
        }
        None => {}
    }

    #[cfg(windows)]
    {
        eprintln!(
            "Windows lacks the required execution and mutation protections. No changes were made."
        );
        ExitCode::FAILURE
    }
    #[cfg(not(windows))]
    {
        run_native_turn(args, tool_memory_embedding).await
    }
}

#[cfg(not(windows))]
async fn run_native_turn(args: Args, tool_memory_embedding: Option<(String, String)>) -> ExitCode {
    if args.confined_read {
        return run_confined_read();
    }
    if args.confined_apply {
        return run_confined_apply();
    }

    // Resolve a requested target before onboarding, helper staging, or any
    // strict-history setup can mutate local state.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Everything below that takes "the project" takes this, not `cwd`.
    // `.polaris/config.toml`, `AGENTS.md`, `.polaris/skills`, `agents/`
    // and the build cache all live at the root; resolving them against the
    // working directory means a run started from `crates/polaris-cli/`
    // silently gets no configuration, no rules, no skills and no agent
    // types, while the state directory (which resolves internally) stays
    // the same. Half the run followed the project and half did not.
    let project_root = polaris_core::project::resolve_root(&cwd);
    let (preflight_state_dir, preflight_sessions_dir) = match preflight_storage_paths(&cwd) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("Can't determine the storage directories: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut config = match polaris_core::config::load(&project_root) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Cannot load configuration: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(mode) = args.workflow_mode {
        mode.apply(&mut config.workflow);
    }
    let configured_history_mode = args
        .history_mode
        .map(polaris_core::conversation_state::HistoryMode::from)
        .unwrap_or(config.history_mode);
    let input_preparation = config.input_preparation;
    let project_id = match polaris_tui::persist::project_identity(&project_root) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("Cannot retrieve the project identifier: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(preflight_data_root) = preflight_sessions_dir.parent() else {
        eprintln!("Cannot locate v2 session storage");
        return ExitCode::FAILURE;
    };
    #[cfg(unix)]
    if let Err(error) =
        resolve_pending_workspace_before_execution(&project_root, &preflight_state_dir)
    {
        eprintln!("Cannot recover workspace files from an interrupted rewind: {error}");
        return ExitCode::FAILURE;
    }
    let resolved_continuation = match resolve_continuation(
        args.resume.as_deref(),
        args.fork.as_deref(),
        preflight_data_root,
        &project_id,
        args.history_mode.map(Into::into),
        configured_history_mode,
        &preflight_sessions_dir,
    ) {
        Ok(continuation) => continuation,
        Err(error) => {
            eprintln!("Cannot open the v2 session: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(unix)]
    if let Some(resolved) = &resolved_continuation
        && let Err(error) = validate_resolved_continuation_workspace(
            resolved,
            &project_root,
            &preflight_state_dir,
            preflight_data_root,
            &project_id,
        )
    {
        eprintln!("Cannot verify execution permission for the saved conversation: {error}");
        return ExitCode::FAILURE;
    }
    let history_mode = resolved_continuation
        .as_ref()
        .map(|continuation| continuation.history_mode)
        .unwrap_or(configured_history_mode);
    if history_mode == polaris_core::conversation_state::HistoryMode::Structured10 {
        // A project or user config cannot start the experimental mode on its own.
        let creates_session = resolved_continuation
            .as_ref()
            .is_none_or(|c| matches!(c.source, ContinuationSource::Legacy(_)));
        if creates_session && args.history_mode != Some(HistoryModeArg::Structured10) {
            eprintln!(
                "structured10 is experimental. New conversations require an explicit --history-mode structured10"
            );
            return ExitCode::FAILURE;
        }
        eprintln!(
            "structured10 is experimental and is not the default pending evaluation with real models."
        );
    }

    let model = selected_model(
        args.model.clone(),
        std::env::var("POLARIS_MODEL").ok(),
        args.prompt.is_none(),
    );
    // A supplied `--effort` is fixed in both modes. One-shot runs retain the
    // historical xhigh default; an interactive TUI leaves this absent so Jev
    // can decide every turn and the footer can begin at `undecided`.
    let initial_effort_name =
        selected_effort(args.effort.as_deref(), args.prompt.is_some()).map(str::to_owned);
    let attempt_parent_id = match polaris_core::session_store::new_session_id() {
        Ok(id) => id,
        Err(error) => {
            eprintln!("Cannot create the parent attempt ID: {error}");
            return ExitCode::FAILURE;
        }
    };
    let attempt_ledger = polaris_provider::attempts::AttemptLedger::with_context(
        polaris_provider::attempts::AttemptContext {
            parent_id: Some(attempt_parent_id.clone()),
            kind: polaris_provider::attempts::AttemptKind::Completion,
        },
    );
    let provider_name = match std::env::var("POLARIS_PROVIDER") {
        Ok(name) => name,
        Err(_) => {
            let has_openai_key = std::env::var("POLARIS_API_KEY").is_ok()
                || polaris_auth::api_key::default_path()
                    .ok()
                    .and_then(|p| polaris_auth::api_key::load_from(&p).ok().flatten())
                    .is_some();
            let has_saved_codex_credentials = polaris_auth::store::default_path()
                .ok()
                .map(|p| p.exists())
                .unwrap_or(false)
                || (!has_openai_key
                    && match account::has_registrations() {
                        Ok(registered) => registered,
                        Err(error) => {
                            eprintln!("{error}");
                            return ExitCode::FAILURE;
                        }
                    });
            default_provider_name(has_openai_key, has_saved_codex_credentials).to_string()
        }
    };
    // Provider setup happens only after continuation preflight. The returned
    // holder remains in scope for the whole turn so a local llama server stays alive.
    let prepared_providers = match provider_startup::prepare(
        &args,
        &config,
        &project_root,
        model,
        initial_effort_name.as_deref(),
        attempt_ledger.clone(),
        provider_name,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(status) => return status,
    };
    let provider_name = prepared_providers.provider_name.clone();
    let model_name = prepared_providers.model_name.clone();
    let mcp_selection_provider = polaris_core::decision_policy::restrict_arbiter(
        config.decision.local_only,
        prepared_providers.mcp_selection_provider.clone(),
    );
    let tier_weights = prepared_providers.tier_weights.clone();
    let provider = prepared_providers.provider.clone();
    let decision_meter = polaris_provider::decision::DecisionMeter::default();
    if config.decision.enabled
        && let Some(fallback) = &config.decision.fallback
    {
        for line in fallback.disclosure(&config.decision) {
            eprintln!("{line}");
        }
    }
    let decider = build_decider(&config.decision).map(|decider| decision_meter.wrap(decider));
    // The same judge weighs delegated tasks. One registration, because both
    // ask the same service the same kind of question.

    // A judge that routes delegations by weight, over four weights that
    // all resolve to the same provider, changes nothing and says nothing.
    // `build_tier_providers` skips a weight that names neither a model nor
    // an effort, so an installation with no `[tier.*]` gets one provider
    // wearing four names — and the ladder's whole point is that they
    // differ. Say it once; do not invent model names as defaults, because
    // which model each weight buys is the project's decision and a wrong
    // guess here spends its money.
    if decider.is_some() && config.tiers.is_empty() {
        eprintln!(
            "Warning: the decider routes delegation across four tiers, but no model or effort is set in [tier.*], so all four resolve to the same provider. Routing has no cost or capability effect until per-tier settings are configured."
        );
    }

    // Priced weights route delegations; roles stay unrouted, because the
    // CLI has no role bindings and a delegation to an unrouted role has to
    // reach the parent rather than be refused.
    let provider: std::sync::Arc<dyn polaris_provider::Provider> = if tier_weights.is_empty() {
        provider
    } else {
        std::sync::Arc::new(polaris_provider::role::RoleProvider::tiers_only(
            provider,
            tier_weights,
        ))
    };

    let mut colocated = vec![provider.clone()];
    colocated.extend(mcp_selection_provider.iter().cloned());
    let decision_colocation =
        polaris_core::decision_capacity::colocation::ColocationSources::new(colocated);
    let decider = decider.map(|decider| decision_colocation.bind(decider));
    let tier_decider: Option<std::sync::Arc<dyn polaris_core::tier::TierDecider>> =
        decider.clone().map(|decider| {
            std::sync::Arc::new(polaris_core::tier::typed::TypedTierDecider::new(decider))
                as std::sync::Arc<dyn polaris_core::tier::TierDecider>
        });

    let state_dir = match polaris_core::project::state_dir(&cwd) {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("Can't determine the state directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let answer_finalizer = if args.prompt.is_none() {
        match build_tui_answer_finalizer(&config, &state_dir, None).await {
            Ok(finalizer) => finalizer,
            Err(error) => {
                eprintln!("Cannot prepare answer completion assessment: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let sessions_dir = match polaris_core::project::sessions_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("Can't determine the sessions directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(data_root) = sessions_dir.parent() else {
        eprintln!("Cannot locate v2 session storage");
        return ExitCode::FAILURE;
    };

    let audit_path = match args.audit {
        Some(p) => p,
        None => match default_audit_path(&cwd) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Can't determine the default audit log path: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let sandbox = match build_sandbox_policy(&cwd, args.sandbox.into()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Can't build the sandbox policy: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut deferred_routine_authority = None;
    let prepared_routine_authority = match args.routine_authority.as_deref() {
        Some(raw) => {
            if !config.workflow.enabled {
                eprintln!("--routine-authority requires an enabled workflow");
                return ExitCode::FAILURE;
            }
            let validation_prompt = args
                .prompt
                .as_deref()
                .unwrap_or("tui deferred routine authority validation");
            match prepare_routine_authority(
                raw,
                validation_prompt,
                &project_root,
                &sandbox,
                &state_dir,
            ) {
                Ok(authority) => Some(authority),
                Err(error) => {
                    eprintln!("Cannot prepare routine authority: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };
    if args.prompt.is_none() {
        deferred_routine_authority = args.routine_authority.clone();
    }
    let prepared_review_authority = match args.review_authority.as_deref() {
        Some(raw) => match prepare_review_authority(raw) {
            Ok(authority) => Some(authority),
            Err(error) => {
                eprintln!("Cannot prepare review authority: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    // The binary that gets re-executed is staged outside the writable root.
    // Skipping this would let anyone who can write to the workspace replace
    // the helper (see the docs on `polaris_sandbox::stage`).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let prepared_helper = match std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|_| polaris_core::isolated_run::PrepareError::InvalidPath)
        .and_then(|executable| {
            polaris_core::isolated_run::PreparedConfinedHelper::prepare(
                &sandbox,
                &state_dir,
                &executable,
            )
        }) {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("Can't prepare the confined helper: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let helper = prepared_helper.helper_path().to_owned();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let helper = match polaris_sandbox::stage::staged_helper(&sandbox, &state_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Can't prepare the confined helper: {e}");
            return ExitCode::FAILURE;
        }
    };

    let approval_policy: ApprovalPolicy = args.approval.into();

    // The ceiling cuts lines from the end and says nothing. Worse, a
    // `~/.polaris/AGENTS.md` with an always-on section of its own takes
    // half the ceiling, so a project file that fitted yesterday can stop
    // fitting because of a file in another directory that the project's
    // own tooling never looks at. Say it once here; `polaris doctor`
    // prints the whole accounting.
    for reach in constitution::reach(&project_root) {
        if reach.dropped > 0 {
            eprintln!(
                "Warning: {} has {} always-on lines, but only {} reach the model ({} lines truncated; {} tokens delivered). See `polaris doctor` for details.",
                reach.path.display(),
                reach.delivered + reach.dropped,
                reach.delivered,
                reach.dropped,
                reach.tokens,
            );
        }
    }

    // Resolve the same two sources once; reads happen before each root request.
    #[cfg(unix)]
    let agents_refresh = match constitution::AgentsRefresh::from_home(&project_root) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("Cannot prepare the AGENTS.md read source: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(not(unix))]
    let constitution = constitution::load(&project_root);
    let environment = constitution::environment_block(&cwd, &project_root, None);

    let history_backend_factory: polaris_tui::HistoryBackendFactory = {
        let embedding = config.embedding.clone();
        #[cfg(target_os = "macos")]
        let runtime_home = match managed_runtime_home() {
            Ok(home) => home,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
        #[cfg(not(target_os = "macos"))]
        let runtime_home = PathBuf::new();
        let provider_name = provider_name.clone();
        let private_root = data_root.to_path_buf();
        let ledger = attempt_ledger.clone();
        let parent_id = attempt_parent_id.clone();
        let decider = decider.clone();
        std::sync::Arc::new(move || {
            if embedding.model_path.is_none() || embedding.revision.is_none() {
                return Err(
                    "strict10 requires [embedding] model_path, revision, and a verified read-only runtime image".into(),
                );
            }
            strict_history(
                &embedding,
                &runtime_home,
                &provider_name,
                &private_root,
                ledger.clone(),
                &parent_id,
                decider.clone(),
            )
        })
    };
    let configured_strict = if matches!(
        history_mode,
        polaris_core::conversation_state::HistoryMode::Strict10
            | polaris_core::conversation_state::HistoryMode::Structured10
    ) {
        match history_backend_factory() {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("Cannot prepare history: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let mut colocated = vec![provider.clone()];
    colocated.extend(mcp_selection_provider.iter().cloned());
    decision_colocation.replace(
        colocated,
        configured_strict.is_some() || tool_memory_embedding.is_some(),
    );
    let continuation = match resolved_continuation {
        Some(resolved) => {
            #[cfg(unix)]
            let prepared = with_cli_management_lease(
                &cwd,
                &state_dir,
                data_root,
                &project_id,
                |lease, effects, rewind| {
                    if let ContinuationSource::V2(target) = &resolved.source {
                        let source = polaris_core::session_store::PersistedSession::open(
                            data_root,
                            &state_dir.join("memory.sqlite3"),
                            &project_id,
                            &target.id,
                        )?;
                        polaris_core::rewind_admission::validate_execution(rewind, &source)?;
                    }
                    let switch = polaris_core::session_switch::SessionSwitchOwner {
                        effects,
                        lease,
                        state_dir: &state_dir,
                    };
                    prepare_continuation_with(
                        resolved,
                        data_root,
                        &state_dir.join("memory.sqlite3"),
                        &project_id,
                        |source| {
                            switch
                            .fork(
                                source,
                                polaris_core::management_effect::ManagementInventoryIdV1::parse(
                                    "F18",
                                )
                                .expect("approved F18 inventory"),
                            )
                            .map_err(|error| std::io::Error::other(error.to_string()))
                        },
                    )
                },
            );
            #[cfg(not(unix))]
            let prepared = prepare_continuation(
                resolved,
                data_root,
                &state_dir.join("memory.sqlite3"),
                &project_id,
            );
            prepared.map(Some)
        }
        None => Ok(None),
    };
    let continuation = match continuation {
        Ok(continuation) => continuation,
        Err(error) => {
            eprintln!("Cannot prepare the v2 session: {error}");
            return ExitCode::FAILURE;
        }
    };
    let discovered = polaris_skills::discover(&project_root, &config.skills_paths);
    for line in format_skipped_skills(&discovered.skipped) {
        eprintln!("{line}");
    }

    let discovered_agents =
        polaris_skills::discover_agent_types(&project_root, &config.agents_paths);
    for s in &discovered_agents.skipped {
        eprintln!("agent type skipped: {s}");
    }

    // Core owns both initial assembly and bounded request-boundary refresh.
    // TUI retains this capability across turns; children keep their own prompts.
    #[cfg(unix)]
    let always_on = match prompt::assemble_always_on("", &environment, &discovered.skills)
        .with_agents_refresh(agents_refresh)
    {
        Ok(prompt) => prompt,
        Err(error) => {
            eprintln!("Cannot load AGENTS.md: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(not(unix))]
    let always_on = prompt::assemble_always_on(&constitution, &environment, &discovered.skills);
    let always_on = match always_on.with_project_root(&project_root) {
        Ok(prompt) => prompt,
        Err(error) => {
            eprintln!("Cannot configure the project reference: {error}");
            return ExitCode::FAILURE;
        }
    };

    let tui_turn_authority: Option<polaris_tui::TurnAuthorityInstaller> =
        deferred_routine_authority.map(|raw| {
            let pending = std::sync::Arc::new(std::sync::Mutex::new(Some((
                raw,
                prepared_review_authority.clone(),
            ))));
            let project_root = project_root.clone();
            let sandbox = sandbox.clone();
            let state_dir = state_dir.clone();
            std::sync::Arc::new(move |session: &mut Session, prompt: &str| {
                let plan = pending
                    .lock()
                    .map_err(|_| "Cannot retrieve routine authority state".to_owned())?
                    .take();
                let Some(plan) = plan else {
                    return Ok(());
                };
                let result = (|| {
                    let prepared = prepare_routine_authority(
                        &plan.0,
                        prompt,
                        &project_root,
                        &sandbox,
                        &state_dir,
                    )?;
                    let owner =
                        install_routine_authority(session, prompt, prepared, &project_root)?;
                    if let Some(review) = plan.1.clone() {
                        apply_review_authority(session, &owner, review)?;
                    }
                    Ok(())
                })();
                if result.is_err() {
                    let mut slot = pending
                        .lock()
                        .map_err(|_| "Cannot restore routine authority state".to_owned())?;
                    *slot = Some(plan);
                }
                result
            }) as polaris_tui::TurnAuthorityInstaller
        });

    match args.prompt.clone() {
        Some(prompt) => {
            // Exactly one handle, shared with whatever subagents `spawn`
            // starts — see `agent::run`'s docs. The root never holds the
            // lock across a turn; `run_loop` takes it per audit record.
            let audit = match AuditLog::open(&audit_path) {
                Ok(a) => std::sync::Arc::new(tokio::sync::Mutex::new(a)),
                Err(e) => {
                    eprintln!("Can't open the audit log: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if let Some(meter) = decider.as_ref().and_then(|held| held.telemetry()) {
                polaris_core::audit::install_decision_observer(&meter, audit.clone());
            }
            let usage_meter = polaris_provider::UsageMeter::default();
            let provider: std::sync::Arc<dyn polaris_provider::Provider> =
                std::sync::Arc::new(usage_meter.wrap(provider.clone()));
            let mcp_selection_provider = mcp_selection_provider.map(|provider| {
                std::sync::Arc::new(usage_meter.wrap(polaris_core::audit::observe_arbitration(
                    provider,
                    audit.clone(),
                ))) as std::sync::Arc<dyn polaris_provider::Provider>
            });
            let mut session = Session::new();
            session.input_preparation = input_preparation;
            session.disable_files_md_auto_regenerate = !config.files_md_auto_regenerate;
            session.approval_auto = config.approval_auto;
            session.tier_decider = tier_decider.clone();
            session.decider = decider.clone();
            // The same independently constructed selector the MCP catalogue
            // uses; the skill catalogue has no reason to want a different
            // one.
            session.light_model = mcp_selection_provider.clone();
            if config.workflow.enabled {
                session.workflow =
                    match polaris_core::workflow::SessionWorkflow::try_new(config.workflow.clone())
                    {
                        Ok(workflow) => Some(workflow),
                        Err(error) => {
                            eprintln!("Cannot start the workflow: {error}");
                            return ExitCode::FAILURE;
                        }
                    };
            }
            if let Some(phase) = args.phase.map(Phase::from) {
                let Some(workflow) = session
                    .workflow
                    .as_ref()
                    .filter(|workflow| workflow.config.enabled)
                else {
                    eprintln!("--phase requires [workflow].enabled = true in configuration");
                    return ExitCode::FAILURE;
                };
                if let Err(error) = workflow.request_phase(phase) {
                    eprintln!("Cannot set the workflow phase: {error}");
                    return ExitCode::FAILURE;
                }
            }
            let project_id = match polaris_tui::persist::project_identity(&project_root) {
                Ok(id) => id,
                Err(error) => {
                    eprintln!("Cannot retrieve the project identifier: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let data_root = match sessions_dir.parent() {
                Some(root) => root,
                None => {
                    eprintln!("Cannot locate v2 session storage");
                    return ExitCode::FAILURE;
                }
            };
            let database = state_dir.join("memory.sqlite3");
            if let Some((history, usage)) = &configured_strict {
                session.strict_history = Some(history.clone());
                session.summary_usage = Some(usage.clone());
            }
            let persistence = match (&args.resume, &args.fork) {
                (Some(_), None) | (None, Some(_)) => continuation
                    .as_ref()
                    .map(|continuation| Ok(continuation.persistence.clone())),
                (None, None) => {
                    #[cfg(unix)]
                    let created = with_cli_management_lease(
                        &cwd,
                        &state_dir,
                        data_root,
                        &project_id,
                        |lease, effects, _rewind| {
                            let switch = polaris_core::session_switch::SessionSwitchOwner {
                                effects,
                                lease,
                                state_dir: &state_dir,
                            };
                            switch
                            .create_new(
                                None,
                                data_root,
                                &database,
                                &project_id,
                                history_mode,
                                session.workflow.as_ref(),
                                // Fresh CLI and TUI /new share the local-session owner.
                                polaris_core::management_effect::ManagementInventoryIdV1::parse(
                                    "T06",
                                )
                                .expect("approved T06 inventory"),
                            )
                            .map_err(|error| std::io::Error::other(error.to_string()))
                        },
                    );
                    #[cfg(not(unix))]
                    let created = polaris_core::session_store::PersistedSession::create(
                        data_root,
                        &database,
                        &project_id,
                        history_mode,
                        session.workflow.as_ref(),
                    );
                    Some(created)
                }
                _ => unreachable!("clap rejects mutually exclusive session flags"),
            };
            let persistence = match persistence {
                None => None,
                Some(Ok(persistence)) => Some(persistence),
                Some(Err(error)) => {
                    eprintln!("Cannot open the v2 session: {error}");
                    return ExitCode::FAILURE;
                }
            };
            if let Some(saved) = &persistence
                && let Err(error) = restore_continuation_workflow(
                    args.resume.as_deref(),
                    args.fork.as_deref(),
                    &mut session,
                    saved,
                    &config.workflow,
                    args.workflow_mode,
                    &discovered.skills,
                    Some(&history_backend_factory),
                )
                .await
            {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
            if (args.resume.is_some() || args.fork.is_some())
                && let Some(mode) = args.workflow_mode
            {
                #[cfg(unix)]
                if let Err(error) = apply_cli_workflow_override(
                    &mut session,
                    mode,
                    &discovered.skills,
                    &cwd,
                    &state_dir,
                    data_root,
                    &project_id,
                ) {
                    eprintln!("Cannot save the workflow change: {error}");
                    return ExitCode::FAILURE;
                }
                #[cfg(not(unix))]
                {
                    let _ = mode;
                    eprintln!("A workflow change requires a management operation");
                    return ExitCode::FAILURE;
                }
            }
            if args.resume.is_none()
                && args.fork.is_none()
                && let Some(persistence) = persistence
                && let Err(error) = session.attach(persistence)
            {
                eprintln!("Cannot attach the v2 session: {error}");
                return ExitCode::FAILURE;
            }
            if let Err(error) = session.restore_verification_checkpoint() {
                eprintln!("Cannot restore the verification checkpoint: {error}");
                return ExitCode::FAILURE;
            }
            if let Some(authority) = prepared_routine_authority.clone() {
                let owner = match install_routine_authority(
                    &mut session,
                    &prompt,
                    authority,
                    &project_root,
                ) {
                    Ok(owner) => owner,
                    Err(error) => {
                        eprintln!("Cannot attach routine authority: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                if let Some(review) = prepared_review_authority.clone()
                    && let Err(error) = apply_review_authority(&mut session, &owner, review)
                {
                    eprintln!("Cannot apply review authority: {error}");
                    return ExitCode::FAILURE;
                }
            }
            let benchmark = benchmark_report::BenchmarkReport::from_environment();
            let root_effort = initial_effort_name.as_deref().unwrap_or("default");
            let history_mode_name = match history_mode {
                polaris_core::conversation_state::HistoryMode::Legacy => "legacy",
                polaris_core::conversation_state::HistoryMode::Strict10 => "strict10",
                polaris_core::conversation_state::HistoryMode::Structured10 => "structured10",
            };
            let tool_memory = match args.tool_memory {
                ToolMemoryArg::Off => "off",
                ToolMemoryArg::History => "reference",
                ToolMemoryArg::Retrieval => "retrieval",
            };
            let (workflow_enabled, workflow_mode) = match session.workflow.as_ref() {
                Some(workflow) => (
                    workflow.config.enabled,
                    match workflow.config.mode {
                        polaris_core::workflow::WorkflowMode::Manual => "manual",
                        polaris_core::workflow::WorkflowMode::Adaptive => "adaptive",
                    },
                ),
                None => (false, "manual"),
            };
            benchmark.configured(benchmark_report::Configured {
                root_model: &model_name,
                root_effort,
                decision_enabled: config.decision.enabled,
                decision_active: decider.is_some(),
                decision_model: config.decision.model.as_deref().unwrap_or("jev-latest"),
                tiers: benchmark_report::effective_tiers(&config.tiers, &model_name, root_effort),
                history_mode: history_mode_name,
                tool_memory,
                workflow_enabled,
                workflow_mode,
            });

            if session.strict_history.is_some()
                && args.resume.is_some()
                && let Err(error) = session.recover_history().await
            {
                eprintln!("Cannot restore strict10 history: {error}");
                if let Some(usage) = &session.summary_usage {
                    let usage = usage.snapshot();
                    eprintln!(
                        "Verified summarization usage: input {} / output {} / total {}. Missing {} / failed {}.",
                        usage.usage.input_tokens,
                        usage.usage.output_tokens,
                        usage.usage.total_tokens,
                        usage.missing_responses,
                        usage.failed_requests
                    );
                }
                return ExitCode::FAILURE;
            }
            session.compaction_threshold = args.compact_at.map(|n| n as usize);
            let session_id = format!(
                "exec-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            if args.remember {
                match polaris_tui::configure_memory(
                    &mut session,
                    &project_root,
                    &state_dir,
                    &session_id,
                ) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("Cannot prepare history storage: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            } else if session.persistence.is_none()
                && let Err(error) = polaris_tui::configure_archive(
                    &mut session,
                    &project_root,
                    &state_dir,
                    &session_id,
                )
            {
                // A v2 session keeps every message in its own append-only
                // log. Without one, compaction is the only writer of the
                // record, and it replaces a prefix with a summary — so the
                // originals have to be put somewhere first.
                eprintln!("Cannot archive history before compaction: {error}");
                return ExitCode::FAILURE;
            }
            if let Some(mode) = args.tool_memory.retention()
                && let Err(error) = polaris_tui::tool_memory::configure_tool_memory(
                    &mut session,
                    &cwd,
                    &state_dir,
                    &session_id,
                    mode,
                    tool_memory_embedding.clone(),
                )
            {
                eprintln!("Cannot prepare tool memory: {error}");
                return ExitCode::FAILURE;
            }
            if args.tool_memory.retention().is_some() {
                eprintln!("Tool memory session: {session_id}");
            }
            #[cfg(unix)]
            let (_management_effects, _project_activity) = {
                let owner =
                    match polaris_core::project_mutation::ProjectMutationOwnerV1::for_project(&cwd)
                    {
                        Ok(owner) => owner,
                        Err(error) => {
                            eprintln!("Cannot verify management operation records: {error}");
                            return ExitCode::FAILURE;
                        }
                    };
                let lease = match owner.try_mutation() {
                    Ok(lease) => lease,
                    Err(error) => {
                        eprintln!("Cannot start management operation recovery: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                let project_root = polaris_core::project::resolve_root(&cwd);
                let store = match polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
                    &project_root,
                    &state_dir,
                    &lease,
                ) {
                    Ok(store) => store,
                    Err(error) => {
                        eprintln!("Cannot recover management operation records: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                match polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_mutation(
                    &project_root,
                    &state_dir,
                    &lease,
                ) {
                    Ok(None) => {}
                    Ok(Some(effect)) => {
                        eprintln!(
                            "Unresolved management operation {} ({}). New input cannot start",
                            effect.request.operation_id, effect.request.inventory
                        );
                        return ExitCode::FAILURE;
                    }
                    Err(error) => {
                        eprintln!("Cannot verify management operation state: {error}");
                        return ExitCode::FAILURE;
                    }
                }
                drop(lease);
                let workspace = match polaris_core::rewind_workspace::WorkspaceOwner::open(
                    &project_root,
                    &state_dir.join("rewind-workspace"),
                    polaris_core::rewind_workspace::RewindLimits::default(),
                ) {
                    Ok(workspace) => workspace,
                    Err(error) => {
                        eprintln!("Cannot verify the rewind transaction: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                let activity = match workspace.try_activity_quiescent() {
                    Ok(activity) => activity,
                    Err(error) => {
                        eprintln!(
                            "Cannot start execution because the workspace is in use: {error}"
                        );
                        return ExitCode::FAILURE;
                    }
                };
                if let Err(error) =
                    polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_activity(
                        &project_root,
                        &state_dir,
                        &activity,
                    )
                    .and_then(|blocker| match blocker {
                        None => Ok(()),
                        Some(_) => Err(polaris_core::sync::composite::C10CompositeErrorV1::Blocked),
                    })
                {
                    eprintln!("Cannot verify management operation state: {error}");
                    return ExitCode::FAILURE;
                }
                (store, Arc::new(activity))
            };
            #[cfg(unix)]
            let mut cli_admission =
                match admission::CliAdmission::reserve(&state_dir, &session, &prompt) {
                    Ok(admission) => admission,
                    Err(error) => {
                        eprintln!("Cannot admit CLI input: {error}");
                        return ExitCode::FAILURE;
                    }
                };
            macro_rules! abort_before_input {
                ($($argument:tt)*) => {{
                    #[cfg(unix)]
                    if let Err(error) = cli_admission.cancel_before_append() {
                        eprintln!("Cannot cancel admitted input: {error}");
                    }
                    eprintln!($($argument)*);
                    return ExitCode::FAILURE;
                }};
            }
            let durable_session_id = session
                .persistence
                .as_ref()
                .and_then(|store| store.snapshot().ok())
                .map(|snapshot| snapshot.state.session_id)
                .unwrap_or_else(|| session_id.clone());
            let ceiling =
                polaris_core::mcp::operation::ExternalCeiling::host_external(args.mcp_external);
            let mut mcp_runtime = None;
            let mut mcp_revision = None;
            let mut selected_prompt_message = None;
            match polaris_core::mcp::local::LocalHost::open(&cwd, false) {
                Ok(host) => {
                    mcp_revision = Some(registry_revision(&host));
                    match host.runtime(&durable_session_id, "exec", ceiling) {
                        Ok(Some((mut runtime, quarantined))) => {
                            if !quarantined.is_empty() && host.root_contains_home() {
                                eprintln!(
                                    "Command-based MCP servers cannot run because the working directory encompasses the entire home. Run from a project folder, such as one containing .git."
                                );
                            }
                            for (server, reason) in quarantined {
                                eprintln!("MCP server {server} is unavailable: {reason}");
                            }
                            host.configure_preselection(
                                &mut runtime,
                                mcp_selection_provider.clone(),
                                decider.clone(),
                            );
                            // Before the first provider request, and only
                            // once: the runtime refuses a second call, and
                            // refuses any call after a request has gone
                            // out. Failing here stops the run rather than
                            // continuing without what was asked for — an
                            // explicitly chosen prompt is part of the
                            // request, not an optimization.
                            if let Some(spec) = args.mcp_prompt.as_deref() {
                                let choice = match mcp_prompt_choice(spec, &args.mcp_prompt_args) {
                                    Ok(choice) => choice,
                                    Err(message) => {
                                        abort_before_input!("{message}");
                                    }
                                };
                                match runtime.host_prompt(&choice, std::future::pending()).await {
                                    Ok(result) => {
                                        let confirmed = result.permits_continuation();
                                        if !confirmed {
                                            abort_before_input!(
                                                "Could not verify MCP prompt {}/{}",
                                                choice.server,
                                                choice.prompt
                                            );
                                        }
                                        selected_prompt_message = Some(result.message);
                                        eprintln!(
                                            "Selected MCP prompt {}/{}",
                                            choice.server, choice.prompt
                                        );
                                    }
                                    Err(error) => {
                                        abort_before_input!("Cannot select an MCP prompt: {error}");
                                    }
                                }
                            }
                            mcp_runtime = Some(runtime);
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!("Cannot prepare MCP servers: {error}"),
                    }
                    session.mcp_setup = Some(std::sync::Arc::new(host.setup_tool(ceiling)));
                }
                Err(error) => eprintln!("Continuing without MCP: {error}"),
            }
            if args.mcp_prompt.is_some() && mcp_runtime.is_none() {
                abort_before_input!("Cannot select an MCP prompt: MCP servers are unavailable");
            }
            #[cfg(unix)]
            if let Err(error) =
                cli_admission.append_and_start(&mut session, &prompt, selected_prompt_message)
            {
                eprintln!("Cannot save and start CLI input: {error}");
                return ExitCode::FAILURE;
            }
            #[cfg(not(unix))]
            if let Some(message) = selected_prompt_message {
                if let Err(error) = session.push_user_with_supplement(&prompt, message) {
                    eprintln!("Cannot save input with an MCP prompt: {error}");
                    return ExitCode::FAILURE;
                }
            } else {
                session.push_user(&prompt);
            }
            if let Err(error) = session.check_persistence() {
                eprintln!("Cannot save input: {error}");
                return ExitCode::FAILURE;
            }
            // 同梱の定型コマンド。projectのrootへ束縛するので、実行した
            // directoryではなくprojectが対象になる。
            session.setting = Some(std::sync::Arc::new(
                polaris_core::recipe::tool::SettingTool::new(polaris_core::project::resolve_root(
                    &cwd,
                )),
            ));
            #[cfg(target_os = "macos")]
            let _environment_turn = {
                let tool = Arc::new(polaris_core::environment::tool::EnvironmentTool::new(
                    polaris_core::project::resolve_root(&cwd),
                    cwd.clone(),
                ));
                let binding = match tool.bind(
                    _management_effects.clone(),
                    Arc::clone(&_project_activity),
                    &session_id,
                ) {
                    Ok(binding) => binding,
                    Err(error) => {
                        eprintln!("{error}");
                        return ExitCode::FAILURE;
                    }
                };
                session.environment = Some(tool);
                binding
            };
            // Driving an application, where the machine allows it. Absent
            // without the accessibility permission, and the `computer`
            // tool is absent with it rather than advertised and refused.
            session.computer =
                polaris_core::computer::ComputerTool::available().map(std::sync::Arc::new);

            // The root owns every child coordinate and capability snapshot.
            // None of these values come from `spawn` text.  The CLI has no
            // durable Desktop projection hook, so this owner is intentionally
            // in-memory for this root run while still routing every spawn
            // through the controlled path.
            let active_task = session
                .workflow
                .as_ref()
                .and_then(|workflow| workflow.state().active_task.map(|task| task.task_id))
                .unwrap_or_else(|| format!("run-task-{session_id}"));
            let sandbox_capability = match sandbox.mode() {
                SandboxMode::ReadOnly => {
                    polaris_core::subagent_control::SandboxCapabilityV1::ReadOnly
                }
                SandboxMode::WorkspaceWrite => {
                    polaris_core::subagent_control::SandboxCapabilityV1::WorkspaceWrite
                }
                SandboxMode::FullAccess => {
                    polaris_core::subagent_control::SandboxCapabilityV1::FullAccess
                }
            };
            let approval_capability = match approval_policy {
                ApprovalPolicy::Never => {
                    polaris_core::subagent_control::ApprovalCapabilityV1::Never
                }
                ApprovalPolicy::OnRequest => {
                    polaris_core::subagent_control::ApprovalCapabilityV1::OnRequest
                }
                ApprovalPolicy::Always => {
                    polaris_core::subagent_control::ApprovalCapabilityV1::TrustedOwnerOnly
                }
            };
            let parent_capabilities = polaris_core::subagent_control::CapabilitySnapshotV1 {
                sandbox: sandbox_capability,
                write_roots: sandbox
                    .writable_roots()
                    .iter()
                    .filter_map(|path| path.to_str().map(str::to_owned))
                    .collect(),
                tool_names: polaris_tools::all_specs()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect(),
                approval: approval_capability,
                mcp: if mcp_runtime.is_some() {
                    polaris_core::subagent_control::McpCapabilityV1::ReadSnapshotsOnly
                } else {
                    polaris_core::subagent_control::McpCapabilityV1::None
                },
            };
            let control = polaris_core::session::RootDelegationControlV1::new(
                polaris_core::session::RootDelegationConfigV1 {
                    session_id: durable_session_id,
                    root_run_id: polaris_core::subagent_control::RootRunId::new(format!(
                        "root-run-{session_id}"
                    ))
                    .expect("host root run id"),
                    root_attempt_id: polaris_core::subagent_control::RootAttemptId::new(format!(
                        "root-attempt-{session_id}"
                    ))
                    .expect("host root attempt id"),
                    active_task_id: polaris_core::subagent_control::TaskId::new(active_task)
                        .expect("host task id"),
                    parent_capabilities,
                    model: model_name.clone(),
                    effort: root_effort.to_owned(),
                },
            );
            match control.and_then(|control| session.install_root_delegation_control(control)) {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("Cannot prepare subagent control: {error}");
                    return ExitCode::FAILURE;
                }
            }

            let mut stop = StopTracker::new(args.max_turns);
            let mut gate = Gate::new(approval_policy);
            let mut approver = TerminalApprover;
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            let outcome = agent::run_with_mcp(
                provider.as_ref(),
                &mut session,
                audit.clone(),
                &mut stop,
                &always_on,
                &discovered.skills,
                &discovered_agents.agent_types,
                provider.clone(),
                config.spawn_concurrency,
                config.spawn_write_concurrency,
                None,
                &mut ctx,
                mcp_runtime.as_mut(),
            )
            .await;
            #[cfg(unix)]
            if let Err(error) = cli_admission.finish(outcome.is_ok()) {
                eprintln!("Cannot save the terminal state of CLI input: {error}");
                return ExitCode::FAILURE;
            }
            // Child MCP read snapshots are in-memory, root-bound evidence.
            // Do not let a one-shot CLI root retain them while shutdown,
            // output formatting, or later process reuse runs.
            let _ = session.revoke_root_mcp_leases().await;
            if let Some(runtime) = mcp_runtime.as_mut()
                && runtime.shutdown().await.is_err()
            {
                eprintln!("Cannot verify that MCP servers stopped.");
            }
            if let Some(before) = mcp_revision
                && let Ok(host) = polaris_core::mcp::local::LocalHost::open(&cwd, false)
                && registry_revision(&host) != before
            {
                mcp::check_unchecked(&host).await;
            }
            if let Some(observation) = &session.memory_observation {
                for line in observation.display_lines() {
                    eprintln!("{line}");
                }
            }
            benchmark.completed(
                outcome.is_ok(),
                decision_meter.snapshot(),
                usage_meter.by_model(),
                usage_meter.snapshot(),
            );
            match outcome {
                Ok(outcome) => {
                    println!("{}", outcome.text);
                    // Same fields, same order as the TUI's `/status`, so a
                    // one-shot run and an interactive one can be compared
                    // without translating between two formats. On stderr,
                    // since stdout is the answer and gets piped.
                    eprintln!(
                        "tokens: in {} / out {} / cache {} / total {} — {} messages",
                        outcome.usage.input_tokens,
                        outcome.usage.output_tokens,
                        outcome.usage.cached_tokens,
                        outcome.usage.total_tokens,
                        session.messages.len(),
                    );
                    sweep_build_cache(&project_root, &config);
                    // Per model, because the weights a run routes between
                    // differ in price by a factor of fifty. A total alone
                    // cannot be converted to money.
                    //
                    // Printed under the meter's own total, not under the line
                    // above it. That line is the conversation's usage — the
                    // main model only — while the meter sees every request the
                    // run made, weights and selection included. Listing the
                    // split beneath a total it does not add up to invites the
                    // reader to subtract two numbers that answer different
                    // questions.
                    let metered = usage_meter.snapshot().usage;
                    eprintln!(
                        "Meter totals: in {} / out {} / cache {} / total {}",
                        metered.input_tokens,
                        metered.output_tokens,
                        metered.cached_tokens,
                        metered.total_tokens,
                    );
                    for (model, usage) in usage_meter.by_model() {
                        eprintln!(
                            "  {model}: in {} / out {} / cache {} / total {}",
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cached_tokens,
                            usage.total_tokens,
                        );
                    }
                    for line in polaris_core::decision_diagnostics::display(
                        &config.decision,
                        decider.as_deref(),
                    ) {
                        eprintln!("{line}");
                    }
                    let judged = decision_meter.snapshot();
                    if judged.reported + judged.missing + judged.failed > 0 {
                        eprintln!(
                            "  Decider: in {} / out {} — responses {} / usage missing {} / failed {}",
                            judged.input_tokens,
                            judged.output_tokens,
                            judged.reported,
                            judged.missing,
                            judged.failed,
                        );
                    }
                    let coverage = usage_meter.snapshot();
                    eprintln!(
                        "Usage measurement: responses {} / missing {} / failed {}. Consumption for missing or failed measurements is unknown.",
                        coverage.reported_responses,
                        coverage.missing_responses,
                        coverage.failed_requests
                    );
                    if let Some(usage) = &session.summary_usage {
                        let usage = usage.snapshot();
                        eprintln!(
                            "Verified summarization usage: input {} / output {} / total {}. Missing {} / failed {}.",
                            usage.usage.input_tokens,
                            usage.usage.output_tokens,
                            usage.usage.total_tokens,
                            usage.missing_responses,
                            usage.failed_requests
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    let coverage = usage_meter.snapshot();
                    eprintln!(
                        "Verified usage through exit: input {} / output {} / total {}. Missing {} / failed {}.",
                        coverage.usage.input_tokens,
                        coverage.usage.output_tokens,
                        coverage.usage.total_tokens,
                        coverage.missing_responses,
                        coverage.failed_requests
                    );
                    if let Some(usage) = &session.summary_usage {
                        let usage = usage.snapshot();
                        eprintln!(
                            "Verified summarization usage: input {} / output {} / total {}. Missing {} / failed {}.",
                            usage.usage.input_tokens,
                            usage.usage.output_tokens,
                            usage.usage.total_tokens,
                            usage.missing_responses,
                            usage.failed_requests
                        );
                    }
                    ExitCode::FAILURE
                }
            }
        }
        None => {
            polaris_tui::run(polaris_tui::RunArgs {
                #[cfg(unix)]
                account_binding: if provider_name == "codex" {
                    account::binding_control()
                } else {
                    None
                },
                provider: provider.clone(),
                mcp_selection_provider,
                decider: decider.clone(),
                decision_configuration: config.decision.clone(),
                decision_colocation,
                tier_decider: tier_decider.clone(),
                answer_finalizer,
                sidechat_auto: config.sidechat_auto,
                tool_names: polaris_tools::all_specs()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect(),
                turn_authority: tui_turn_authority,
                provider_name: provider_name.clone(),
                model_name: model_name.clone(),
                initial_effort_name: initial_effort_name.clone(),
                cwd: cwd.clone(),
                state_dir,
                sessions_dir,
                audit_path,
                max_turns: args.max_turns,
                remember: args.remember,
                tool_memory: args.tool_memory.retention(),
                tool_memory_embedding,
                compact_at: args.compact_at.map(|n| n as usize),
                sandbox,
                helper,
                approval_policy,
                mcp_external: args.mcp_external,
                always_on: &always_on,
                skills: &discovered.skills,
                agent_types: &discovered_agents.agent_types,
                spawn_concurrency: config.spawn_concurrency,
                spawn_write_concurrency: config.spawn_write_concurrency,
                files_md_auto_regenerate: config.files_md_auto_regenerate,
                approval_auto: config.approval_auto,
                workflow_config: config.workflow,
                initial_phase: args.phase.map(Phase::from),
                history_mode,
                input_preparation,
                continuation,
                workflow_mode_overridden: args.workflow_mode.is_some(),
                strict_history: configured_strict
                    .as_ref()
                    .map(|(history, _)| history.clone()),
                summary_usage: configured_strict.as_ref().map(|(_, usage)| usage.clone()),
                history_backend_factory: Some(history_backend_factory),
            })
            .await
        }
    }
}

/// 隔離read専用。要求と応答を有界にし、失敗本文をstderrへ出さない。
fn run_confined_read() -> ExitCode {
    use std::io::Write;
    let reply = polaris_core::agent::serve_confined_read(std::io::stdin().lock());
    match std::io::stdout().lock().write_all(&reply) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Entry point for one confined JSON mutation.
fn run_confined_apply() -> ExitCode {
    use std::io::Read;

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("Can't read stdin: {e}");
        return ExitCode::FAILURE;
    }
    let mutation: polaris_sandbox::Mutation = match serde_json::from_str(&buf) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Can't parse the operation: {e}");
            return ExitCode::FAILURE;
        }
    };
    match polaris_sandbox::helper::apply(&mutation) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            // The kind of failure (whether the OS refused it, or the
            // request itself was malformed) can only be judged on this
            // side, where errno is available. We put the verdict on
            // stderr, marked via `to_wire`, so that the parent
            // (`run_mutation`) doesn't confuse the two. We don't add to
            // the exit-code contract.
            eprintln!("{}", e.to_wire());
            ExitCode::FAILURE
        }
    }
}

/// Builds the project's state directory. Never placed inside the working
/// directory: the moment the repository is `git add -A`'d, even a single
/// line where redaction was missed could end up mixed straight into a
/// commit (secret_screen calls itself a safety net, not a guarantee), so
/// this always lives under the home directory.
///
/// Both the audit log (default path) and the confined helper's staging
/// location share this directory. If it were assembled independently in
/// each place, the two paths could split into different directories, so
/// the assembly is consolidated here into one place.
///
/// The identifier is built from the project root that
/// `project::resolve_root` returns, not the working directory — the same
/// resolution used to derive the writable root. Back when the working
/// directory was hashed directly, the same project would land in a
/// different directory just because it was launched from deep inside the
/// repository, splitting the audit log and the 30MB helper copy per
/// launch location. The audit log is the record of reconstruction this
/// milestone set up, and if its history is split, "look in one place to
/// see the whole story" no longer holds.
///
/// The default audit log path. Placed directly under the state directory.
fn default_audit_path(cwd: &Path) -> io::Result<PathBuf> {
    Ok(polaris_core::project::state_dir(cwd)?.join("audit.jsonl"))
}

/// Builds the sandbox policy for a run. The writable root is derived from
/// the project root, not `cwd` as-is — using `cwd` directly would change
/// what's writable just because you launched from deep inside the
/// repository (see the docs on `polaris_core::project::resolve_root`).
/// Shared between the normal run path and the standalone `sandbox`
/// subcommand so the two can never disagree about what a given
/// `--sandbox` mode actually allows.
fn build_sandbox_policy(
    cwd: &Path,
    sandbox_mode: SandboxMode,
) -> Result<SandboxPolicy, polaris_sandbox::SandboxError> {
    let root = polaris_core::project::resolve_root(cwd);
    let mut writable_roots: Vec<PathBuf> = if sandbox_mode == SandboxMode::WorkspaceWrite {
        vec![root]
    } else {
        Vec::new()
    };
    // The per-user temporary directory, the way codex grants it. The child
    // already receives `TMPDIR`; until now it could not write there, and on
    // macOS `/usr/bin/git` and `/usr/bin/python3` are `xcrun` shims that
    // write `$TMPDIR/xcrun_db-*` before doing anything else. Denied that,
    // they fall back to `xcodebuild` and emit about 800 bytes of
    // `DVTToolchain` complaints per invocation, and `git` exits 128 on top
    // — 10,884 bytes per shell task measured on 2026-09-21 once the history
    // re-sends are counted, about a quarter of what that task bills. It
    // also made results lie: a run whose assertions all printed OK opened
    // with "the command failed with exit code 128".
    //
    // `TMPDIR` only, never the `/tmp` that `std::env::temp_dir` falls back
    // to: the macOS per-user directory is not readable by other users and
    // `/tmp` is.
    if sandbox_mode == SandboxMode::WorkspaceWrite
        && let Some(tmpdir) = std::env::var_os("TMPDIR")
        && !tmpdir.is_empty()
    {
        let tmpdir = PathBuf::from(tmpdir);
        if tmpdir.is_absolute() && tmpdir.is_dir() {
            writable_roots.push(tmpdir);
        }
    }
    SandboxPolicy::new(sandbox_mode, &writable_roots)
}

/// `polaris common-tests`. The host fixes the project root, sandbox and
/// timeout; the plan supplies only one executable invocation and bounded
/// black-box assertions. Failed confinement is reported as unavailable and
/// never retried outside the sandbox.
fn run_common_tests(plan: &str, sandbox_mode: SandboxMode) -> ExitCode {
    let body = match read_common_test_plan(plan) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("Cannot read the plan: {error}");
            return ExitCode::FAILURE;
        }
    };
    let plan: polaris_core::common_tests::Plan = match serde_json::from_str(&body) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("Invalid plan format: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(refused) = plan.check() {
        eprintln!("Plan refused: {refused:?}");
        return ExitCode::FAILURE;
    }
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("Cannot resolve the execution directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let write_root = polaris_core::project::resolve_root(&cwd);
    let policy = match build_sandbox_policy(&cwd, sandbox_mode) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("Cannot prepare the sandbox: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut outcome = polaris_core::common_tests::run(
        &plan,
        polaris_core::common_tests::RunConfig {
            policy: &policy,
            cwd: &cwd,
            write_root: &write_root,
            timeout: std::time::Duration::from_secs(120),
        },
    );
    recover_common_test_cleanup(&mut outcome);
    match serde_json::to_string_pretty(&outcome.report) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("Cannot represent the common case result: {error}");
            return ExitCode::FAILURE;
        }
    }
    if matches!(
        &outcome.report,
        polaris_core::common_tests::RunReport::Passed { .. }
    ) && !outcome.has_pending_cleanup()
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

const STATIC_COMPONENT_MANIFEST_DESCRIPTION: &str =
    "The component Cargo manifest is readable and declares one package.";
const STATIC_COMPONENT_MANIFEST_FAILURE: &str =
    "The component Cargo manifest is missing, unreadable, aliased, or lacks a package name.";
const STATIC_COMPONENT_MANIFEST_RULE: &str = "component-cargo-manifest-v1";

fn run_static_component_manifest() -> ExitCode {
    let manifest = Path::new("Cargo.toml");
    let satisfied = std::fs::symlink_metadata(manifest)
        .ok()
        .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .and_then(|_| std::fs::read_to_string(manifest).ok())
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .is_some();
    let finding = serde_json::json!({
        "rule_id": STATIC_COMPONENT_MANIFEST_RULE,
        "path": "Cargo.toml",
        "line": null,
        "symbol": null,
        "introduced_by_current_diff": true,
        "baseline": false
    });
    let response = serde_json::json!({
        "schema_version": 1,
        "state": if satisfied { "satisfied" } else { "violated" },
        "findings": if satisfied { Vec::<serde_json::Value>::new() } else { vec![finding] },
        "error_kind": null
    });
    println!("{response}");
    if satisfied {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// A standalone CLI invocation has no durable process owner after it returns
/// to the shell. Recover only the exact child reported by this run before
/// returning; it never transfers ownership to the in-process fallback registry
/// (which vanishes with this CLI process), drains another session's registry,
/// or starts another case.
fn recover_common_test_cleanup(outcome: &mut polaris_core::common_tests::RunOutcome) {
    if !outcome.has_pending_cleanup() {
        return;
    }
    // Every owner tick is nonblocking. There is deliberately no terminal
    // deadline: after one, `park_cleanup()` would only retain the compound
    // owner in this short-lived process, leaving its child and scratch
    // unreachable once the CLI exits. The periodic notice makes a delayed
    // group reap visible without claiming it has completed.
    let started = std::time::Instant::now();
    let mut last_notice = started;
    let child_id = outcome.cleanup_mut().and_then(|cleanup| cleanup.child_id());
    eprintln!(
        "Reaping common case child processes{}. The process will not exit until cleanup completes.",
        child_id
            .map(|id| format!(" (pid {id})"))
            .unwrap_or_default()
    );
    loop {
        match outcome
            .cleanup_mut()
            .expect("pending cleanup remains owned")
            .try_stop_reap_and_cleanup()
        {
            Ok(true) => {
                drop(outcome.take_cleanup());
                return;
            }
            Ok(false) => {}
            Err(error) => {
                // A signal or filesystem error is not evidence that the
                // process group disappeared. Retain the owner and retry.
                if last_notice.elapsed() >= std::time::Duration::from_secs(5) {
                    eprintln!(
                        "Retrying common case cleanup ({} seconds elapsed): {error}",
                        started.elapsed().as_secs()
                    );
                    last_notice = std::time::Instant::now();
                }
            }
        }
        if last_notice.elapsed() >= std::time::Duration::from_secs(5) {
            eprintln!(
                "Waiting for common case cleanup ({} seconds elapsed). Child processes and workspace are retained.",
                started.elapsed().as_secs()
            );
            last_notice = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn read_common_test_plan(plan: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(polaris_core::common_tests::MAX_PLAN_BYTES + 1);
    let mut input: Box<dyn Read> = if plan == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(std::fs::File::open(plan).map_err(|error| format!("{plan}: {error}"))?)
    };
    input
        .by_ref()
        .take((polaris_core::common_tests::MAX_PLAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > polaris_core::common_tests::MAX_PLAN_BYTES {
        return Err(format!(
            "The plan cannot exceed {} bytes",
            polaris_core::common_tests::MAX_PLAN_BYTES
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("Not UTF-8: {error}"))
}

/// `polaris doctor`. Diagnoses the local installation: read-only checks
/// only, and never prints the credential contents themselves — only
/// whether each file exists and parses.
async fn run_doctor() -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    println!("polaris doctor\n");

    println!("Decision capacity (read-only):");
    let capacity = polaris_core::decision_capacity::observation::host_memory();
    for line in polaris_core::decision_capacity::observation::diagnostic_lines(&capacity) {
        println!("  {line}");
    }
    println!();

    println!("Credentials:");
    match polaris_auth::store::default_path() {
        Ok(p) => {
            let status = if !p.exists() {
                "not found".to_string()
            } else {
                match std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|s| serde_json::from_str::<polaris_auth::Credentials>(&s).ok())
                {
                    Some(_) => "present, readable".to_string(),
                    None => "present, but could not be parsed".to_string(),
                }
            };
            println!("  {} (ChatGPT sign-in): {status}", p.display());
        }
        Err(e) => println!("  Can't determine the credentials path: {e}"),
    }
    match polaris_auth::api_key::default_path() {
        Ok(p) => {
            let status = match polaris_auth::api_key::load_from(&p) {
                Ok(Some(_)) => "present, readable".to_string(),
                Ok(None) => "not found".to_string(),
                Err(e) => format!("present, but could not be read: {e}"),
            };
            println!("  {} (API key): {status}", p.display());
        }
        Err(e) => println!("  Can't determine the API key path: {e}"),
    }
    let env_key_set = std::env::var("POLARIS_API_KEY").is_ok();
    println!(
        "  POLARIS_API_KEY environment variable: {}",
        if env_key_set { "set" } else { "not set" }
    );

    println!("\nProvider resolution:");
    match std::env::var("POLARIS_PROVIDER") {
        Ok(p) => println!("  POLARIS_PROVIDER is set explicitly: {p}"),
        Err(_) => {
            let has_openai_key = env_key_set
                || polaris_auth::api_key::default_path()
                    .ok()
                    .and_then(|p| polaris_auth::api_key::load_from(&p).ok().flatten())
                    .is_some();
            let has_saved_codex_credentials = polaris_auth::store::default_path()
                .ok()
                .map(|p| p.exists())
                .unwrap_or(false);
            let chosen = default_provider_name(has_openai_key, has_saved_codex_credentials);
            println!("  POLARIS_PROVIDER is not set; would default to: {chosen}");
        }
    }

    println!("\nSandbox:");
    for (label, mode) in [
        ("read-only", SandboxMode::ReadOnly),
        ("workspace-write", SandboxMode::WorkspaceWrite),
    ] {
        match build_sandbox_policy(&cwd, mode) {
            Ok(policy) => println!("  {label}: available ({})", policy.describe()),
            Err(e) => println!("  {label}: NOT available ({e})"),
        }
    }

    let project_root = polaris_core::project::resolve_root(&cwd);
    let storage_paths = preflight_storage_paths(&cwd);
    println!("\nState:");
    match &storage_paths {
        Ok((state_dir, _)) => println!("  state directory: {}", state_dir.display()),
        Err(error) => println!("  Can't determine the state directory: {error}"),
    }

    #[cfg(unix)]
    {
        println!("\nRewind candidates:");
        match &storage_paths {
            Ok((state_dir, sessions_dir)) => match sessions_dir.parent() {
                Some(data_root) => {
                    for line in doctor_rewind_candidate_lines(&project_root, state_dir, data_root) {
                        println!("  {line}");
                    }
                }
                None => println!("  Cannot locate storage"),
            },
            Err(error) => println!("  Cannot locate storage: {error}"),
        }
    }

    // Which project rules actually reach the model. The ceiling truncates
    // from the end in silence, and a global AGENTS.md halves the project's
    // share, so "I wrote the rule down" and "the model was told" are two
    // different facts. This prints the second one.
    println!("\nAGENTS.md (always-on):");
    let reached = polaris_core::constitution::reach(&project_root);
    if reached.is_empty() {
        println!("  no always-on section anywhere. polaris reads only a `## Always on`");
        println!("  heading or a <!-- polaris:always-on --> ... <!-- /polaris:always-on -->");
        println!("  pair; text outside those is never sent. `/init` writes a starting point.");
    }
    for reach in &reached {
        let verdict = if reach.dropped == 0 {
            "all lines reach the model".to_string()
        } else {
            format!("{} line(s) DROPPED, never sent", reach.dropped)
        };
        println!(
            "  {}: {} line(s) / {} tokens reach the model \u{2014} {verdict}",
            reach.path.display(),
            reach.delivered,
            reach.tokens,
        );
    }
    if reached.len() == 2 {
        println!(
            "  both files have a section, so the global one is capped at {} tokens",
            polaris_core::constitution::CONSTITUTION_LIMIT / 2
        );
        println!(
            "  and the project gets whatever is left of {}.",
            polaris_core::constitution::CONSTITUTION_LIMIT
        );
    }

    ExitCode::SUCCESS
}

#[cfg(unix)]
fn doctor_rewind_candidate_lines(
    project_root: &Path,
    state_dir: &Path,
    data_root: &Path,
) -> Vec<String> {
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};

    let project_id = match polaris_tui::persist::project_identity(project_root) {
        Ok(id) => id,
        Err(error) => return vec![format!("Cannot verify the project: {error}")],
    };
    let store_root = state_dir.join("rewind-workspace");
    let mut lines = Vec::new();
    let owner = match WorkspaceOwner::open_existing_read_only(
        project_root,
        &store_root,
        RewindLimits::default(),
    ) {
        Ok(Some(owner)) => Some(owner),
        Ok(None) => {
            lines.push(
                "Storage not created (candidate publication evidence cannot be verified)".into(),
            );
            None
        }
        Err(error) => {
            lines.push(format!(
                "Cannot verify storage: {}",
                error.to_string().escape_debug()
            ));
            None
        }
    };
    let diagnostics = if let Some(owner) = owner.as_ref() {
        polaris_core::rewind_admission::candidate_diagnostics(owner, data_root, &project_id)
    } else {
        polaris_core::rewind_admission::candidate_diagnostics_without_workspace(
            data_root,
            &project_id,
        )
    };
    if diagnostics.is_empty() {
        if owner.is_some() {
            lines.push("No refused or indeterminate candidates".into());
        }
        return lines;
    }
    lines.push(format!(
        "{} refused or indeterminate candidates",
        diagnostics.len()
    ));
    for diagnostic in diagnostics.iter().take(128) {
        lines.push(format!(
            "{}: {}",
            diagnostic.id.escape_debug(),
            diagnostic.reason.escape_debug()
        ));
    }
    if diagnostics.len() > 128 {
        lines.push(format!(
            "{} more. Inspect storage while processing is stopped",
            diagnostics.len() - 128
        ));
    }
    lines
}

#[cfg(all(test, unix))]
#[path = "tests/doctor_rewind_candidate_tests.rs"]
mod doctor_rewind_candidate_tests;

/// When `POLARIS_PROVIDER` isn't set, decides which provider arm to try
/// first. Prefers `openai` if a key is available (env var or the saved
/// `api_key.json`), since that's the arm onboarding itself lives on. If no
/// openai key exists but the user already signed in with ChatGPT (saved
/// `~/.polaris/auth.json`, from `polaris login` or onboarding, or a registered
/// account), prefer
/// `codex` instead of re-triggering onboarding from scratch. With neither
/// credential present, default to `openai` so onboarding fires.
fn default_provider_name(has_openai_key: bool, has_saved_codex_credentials: bool) -> &'static str {
    if !has_openai_key && has_saved_codex_credentials {
        "codex"
    } else {
        "openai"
    }
}

/// Formats each skipped skill as one line. Even if a single skill is
/// broken, this display logic — kept separate from the eprintln! call so
/// it can be tested without side effects — makes sure the fact that it
/// couldn't be read, and why, is never silently swallowed from the user.
fn format_skipped_skills(skipped: &[polaris_skills::Skipped]) -> Vec<String> {
    skipped
        .iter()
        .map(|s| format!("Can't read skill: {s}"))
        .collect()
}

#[cfg(test)]
#[path = "tests/prompt_choice_tests.rs"]
mod prompt_choice_tests;

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/tool_memory_settings_tests.rs"]
mod tool_memory_settings_tests;
