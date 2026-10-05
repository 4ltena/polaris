//! Entry point for the `polaris` binary. Decides the endpoint from environment
//! variables, assembles the always-on context, and runs the agent loop once.

use std::io::{self, Read, Write};
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
mod benchmark_report;
mod compare;
mod decided;
mod handoff;
mod mcp;
mod memory;
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
  POLARIS_PROVIDER  openai (default) or codex. codex uses the credentials from `polaris login`.
  POLARIS_API_KEY   Required when provider=openai. API key for an OpenAI-compatible endpoint.
  POLARIS_BASE_URL  Used when provider=openai; defaults to https://api.openai.com/v1
  POLARIS_MODEL     Defaults to gpt-6-astra; interactive effort is judged automatically
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

    /// 実行モデル。省略時はPOLARIS_MODEL、未設定ならgpt-6-astra。
    #[arg(long)]
    model: Option<String>,

    /// 送信に使う登録ID。省略時は登録が1件ならそれ、複数なら利用枠で選ぶ。
    #[arg(long)]
    account: Option<String>,

    /// 推論強度。対話TUIでは省略時に自動判定し、`-p`ではxhighを使う。
    ///
    /// この既定はorchestrator——rootの実行自身——の強度である。委譲先の
    /// 強度は`[tier.*]`が決め、そこで強度を書かなかった段だけがこの値を
    /// 引き継ぐ。`max`はResponses API限定だが、polarisの主経路は
    /// `/responses`なので送れる。`ultra`はAPIに無いので受け付けない。
    #[arg(long, value_parser = ["low", "medium", "high", "xhigh", "max"])]
    effort: Option<String>,

    /// 圧縮前の履歴をローカル保存し、プロジェクト別の検索索引を作る。
    #[arg(long)]
    remember: bool,

    /// ツール結果の退避。既定はoff。履歴保存の--rememberとは独立。
    #[arg(long, value_enum, default_value_t = ToolMemoryArg::Off)]
    tool_memory: ToolMemoryArg,

    /// ツール記憶用の埋め込みURL。モデルとの同時指定と記憶の有効化が必要。
    #[arg(long, requires = "tool_memory_embedding_model")]
    tool_memory_embedding_url: Option<String>,

    /// ツール記憶用の埋め込みモデル。URLとの同時指定が必要。
    #[arg(long, requires = "tool_memory_embedding_url")]
    tool_memory_embedding_model: Option<String>,

    /// 自動圧縮を開始する推定トークン数。省略時は既存の200,000。
    #[arg(long, value_parser = clap::value_parser!(u32).range(1000..))]
    compact_at: Option<u32>,

    /// Hosted Web検索。接続先の上限契約が確認できるまでcached/liveは拒否する。
    #[arg(long, value_enum, default_value_t = WebSearchArg::Disabled)]
    web_search: WebSearchArg,

    /// workflowを有効にしたセッションの作業段階。
    #[arg(long, value_enum, conflicts_with_all = ["resume", "fork"])]
    phase: Option<PhaseArg>,

    /// Host-authored routine-change authority for this exact prompt.
    #[arg(long, value_name = "JSON")]
    routine_authority: Option<String>,

    /// Host-authored review for the exact published verification checkpoint.
    #[arg(long, value_name = "JSON", requires = "routine_authority")]
    review_authority: Option<String>,

    /// 保存済みworkflowの実行方式を明示的に変更する。resume/forkでは保存後に反映する。
    #[arg(long, value_enum)]
    workflow_mode: Option<WorkflowModeArg>,

    /// 履歴の保存方式。strict10は固定したローカル埋め込み設定を必要とする。
    #[arg(long, value_enum)]
    history_mode: Option<HistoryModeArg>,

    /// 保存済みv2セッションを再開する。旧JSONLは新しいv2 UUIDへ一度だけ取り込む。
    #[arg(long, conflicts_with = "fork")]
    resume: Option<String>,

    /// 指定v2セッションを新しいUUIDへ分岐して実行する。
    #[arg(long, conflicts_with = "resume")]
    fork: Option<String>,

    /// Run as a confined child that reads one mutation operation from stdin
    /// and executes it. Internal use only; not meant to be invoked directly
    /// by users.
    #[arg(long, hide = true)]
    confined_apply: bool,

    /// OS隔離された子で有界readを一件実行する内部入口。
    #[arg(long, hide = true, conflicts_with = "confined_apply")]
    confined_read: bool,

    /// The sandbox policy.
    #[arg(long, value_enum, default_value_t = SandboxModeArg::WorkspaceWrite)]
    sandbox: SandboxModeArg,

    /// The approval boundary policy.
    #[arg(long, value_enum, default_value_t = ApprovalPolicyArg::OnRequest)]
    approval: ApprovalPolicyArg,

    /// MCPのツール実行と登録を許可する。操作ごとに確認する。
    #[arg(long)]
    mcp_external: bool,

    /// 実行前にMCPサーバーのpromptを1つ選ぶ。`サーバー名/prompt名`で指定する。
    /// 選べるのは1回だけで、最初の要求より前に限る。
    #[arg(long, value_name = "SERVER/PROMPT")]
    mcp_prompt: Option<String>,

    /// `--mcp-prompt`へ渡す引数。`key=value`で、複数回指定できる。
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
        .map_err(|error| format!("--routine-authorityが不正です: {error}"))?;
    if wire.schema_version != 1 {
        return Err("--routine-authorityのschema_versionは1だけを受け付けます".into());
    }
    if sandbox.mode() != SandboxMode::WorkspaceWrite {
        return Err("--routine-authorityにはworkspace-write sandboxが必要です".into());
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
        return Err("component_rootはproject内の相対pathで指定してください".into());
    }
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("project rootを確認できません: {error}"))?;
    let component_root = project_root
        .join(&wire.component_root)
        .canonicalize()
        .map_err(|error| format!("component_rootを確認できません: {error}"))?;
    if component_root == project_root || !component_root.starts_with(&project_root) {
        return Err("component_rootには単一packageのrootが必要です".into());
    }
    if !sandbox
        .writable_roots()
        .iter()
        .any(|root| component_root.starts_with(root))
    {
        return Err("component_rootがsandboxのwrite root外です".into());
    }
    let manifest_path = component_root.join("Cargo.toml");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("component manifestを読めません: {error}"))?;
    let manifest: toml::Value = toml::from_str(&manifest_text)
        .map_err(|error| format!("component manifestが不正です: {error}"))?;
    let package_name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .ok_or("component_rootには単一Cargo packageが必要です")?
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
        return Err("--routine-authorityはroutine変更だけに使えます".into());
    }
    if wire.criteria.is_empty() || wire.criteria.len() > 32 {
        return Err("--routine-authorityのcriteria件数が不正です".into());
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
                        "static criterionは登録済みcomponent manifest契約だけを受け付けます".into(),
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
                    .ok_or("common_case criterionにはcommon_runner_planが必要です")?;
                plan.check()
                    .map_err(|error| format!("common runner planが不正です: {error:?}"))?;
                let timeout_secs = criterion
                    .timeout_secs
                    .filter(|seconds| (1..=300).contains(seconds))
                    .ok_or("common runner timeoutは1..=300秒で指定してください")?;
                (
                    polaris_core::workflow::CriterionEvidenceKindV1::CommonCase,
                    Some((plan, timeout_secs)),
                )
            }
            RoutineCriterionEvidenceWireV1::ExistingTest => {
                if criterion.common_runner_plan.is_some() || criterion.timeout_secs.is_some() {
                    return Err(
                        "existing_test criterionにはcommon runner設定を指定できません".into(),
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
    let (existing_test_executor, verification_preparation) =
        match (has_existing_test, wire.existing_test) {
            (false, None) => (
                None,
                polaris_core::verification::HostVerificationPreparationV1::default(),
            ),
            (false, Some(_)) => {
                return Err("existing_test criterionなしでexecutorは登録できません".into());
            }
            (true, None) => {
                return Err("existing_test criterionにはhost executorが必要です".into());
            }
            (true, Some(config)) => {
                if !config.cargo_program.is_absolute() || !(1..=300).contains(&config.timeout_secs)
                {
                    return Err("existing-test executor設定が不正です".into());
                }
                config
                    .inventory
                    .validate()
                    .map_err(|_| "existing-test inventoryが不正です")?;
                if !config.inventory.complete
                    || config.inventory.framework != "rust"
                    || config
                        .inventory
                        .entries
                        .iter()
                        .any(|entry| entry.package != package_name)
                {
                    return Err(
                        "existing-test inventoryは単一componentの完全Rust目録が必要です".into(),
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
        .map_err(|_| "routine authorityをhash化できません")?
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
    let program = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("static analyzer実体を確認できません: {error}"))?;
    let program_bytes = std::fs::read(&program)
        .map_err(|error| format!("static analyzer実体を読めません: {error}"))?;
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
                .map_err(|error| format!("static analyzer runtimeを確認できません: {error}"))?;
            if !runtime_roots.contains(&canonical) {
                runtime_roots.push(canonical);
            }
        }
    }
    let state_root = static_state_root
        .canonicalize()
        .map_err(|error| format!("static analyzer state rootを確認できません: {error}"))?;
    let component_key =
        polaris_core::conversation_state::content_hash(component_root.to_string_lossy().as_bytes());
    let container = state_root.join(format!("static-adapter-{}", &component_key[..16]));
    let home = container.join("home");
    let tmpdir = container.join("tmp");
    prepare_static_private_directory(&container)?;
    prepare_static_private_directory(&home)?;
    prepare_static_private_directory(&tmpdir)?;
    let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, component_root, &runtime_roots)
        .and_then(|policy| policy.with_isolated_private_environment(&container, &home, &tmpdir))
        .and_then(SandboxPolicy::require_process_containment)
        .map_err(|error| format!("static analyzer sandboxを準備できません: {error}"))?;
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
        return Err("static analyzer private scratchはこのplatformで利用できません".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "static analyzer private scratchを作成できません: {error}"
                ));
            }
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("static analyzer private scratchを保護できません: {error}"))?;
        let metadata = path
            .symlink_metadata()
            .map_err(|error| format!("static analyzer private scratchを確認できません: {error}"))?;
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("static analyzer private scratchを確認できません: {error}"))?;
        if !metadata.is_dir() || metadata.mode() & 0o7077 != 0 || canonical != path {
            return Err("static analyzer private scratchのidentityが不正です".into());
        }
        Ok(())
    }
}

fn prepare_review_authority(raw: &str) -> Result<ReviewAuthorityWireV1, String> {
    let wire: ReviewAuthorityWireV1 = serde_json::from_str(raw)
        .map_err(|error| format!("--review-authorityが不正です: {error}"))?;
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    if wire.schema_version != 1 {
        return Err("--review-authorityのschema_versionは1だけを受け付けます".into());
    }
    if !valid_id(&wire.checkpoint_id) || !valid_id(&wire.reviewer_id) {
        return Err("--review-authorityのidentityが不正です".into());
    }
    if wire.findings.iter().any(|finding| {
        finding.disposition == polaris_core::workflow::ReviewFindingDispositionV1::Open
    }) {
        return Err("--review-authorityはopen findingを受け付けません".into());
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
        .map_err(|error| format!("--review-authorityのfindingが不正です: {error}"))?;
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
        .ok_or("--routine-authorityには永続sessionが必要です")?;
    let active_reference = session
        .workflow
        .as_ref()
        .and_then(|workflow| workflow.state().active_change_contract.clone());
    if active_reference.is_none() {
        let workflow = session
            .workflow
            .as_ref()
            .ok_or("--routine-authorityには有効なworkflowが必要です")?;
        if workflow.state().phase != Phase::Specify {
            workflow.request_phase(Phase::Specify)?;
            session.checkpoint().map_err(|error| error.to_string())?;
        }
    }
    let authorization = if let Some(reference) = active_reference {
        let contract = persistence
            .load_change_contract(&reference)
            .map_err(|error| format!("active change contractを読めません: {error}"))?;
        if contract.eligibility != prepared.eligibility
            || contract.verification != prepared.verification
        {
            return Err("保存済みcontractが--routine-authorityと一致しません".into());
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
    Err("cargo metadata用の固定cargo実体を確認できません".into())
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
        .map_err(|error| format!("workspace rootを確認できません: {error}"))?;
    let component_manifest = component_root
        .join("Cargo.toml")
        .canonicalize()
        .map_err(|error| format!("component manifestを確認できません: {error}"))?;
    let cargo_program = cargo_program
        .canonicalize()
        .map_err(|error| format!("cargo実体を確認できません: {error}"))?;
    let cargo_bytes = read_regular_resolution_input(&cargo_program, "cargo実体")?;
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
        .map_err(|error| format!("cargo metadataを実行できません: {error}"))?;
    if !output.status.success() {
        return Err("cargo metadataが完全なlocked/offline closureを返しませんでした".into());
    }
    if output.stdout.is_empty() || output.stdout.len() > MAX_METADATA_BYTES {
        return Err("cargo metadata出力が検証上限外です".into());
    }
    let metadata: CargoMetadataBindingV1 =
        serde_json::from_slice(&output.stdout).map_err(|_| "cargo metadata出力を解釈できません")?;
    let workspace_root = PathBuf::from(&metadata.workspace_root)
        .canonicalize()
        .map_err(|error| format!("cargo workspace rootを確認できません: {error}"))?;
    if workspace_root != project_root {
        return Err("cargo metadataのworkspace rootがproject rootと一致しません".into());
    }
    let resolve = metadata
        .resolve
        .ok_or("cargo metadataに完全なresolve graphがありません")?;
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
        return Err("cargo metadataのpackage/resolve graphが不完全です".into());
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
        return Err("component packageをcargo metadataで一意に解決できません".into());
    };
    let mut pending = vec![component_id.clone()];
    let mut reachable = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        let dependencies = nodes
            .get(&id)
            .ok_or("cargo metadataのdependency nodeが不足しています")?;
        for dependency in dependencies {
            if !packages.contains_key(dependency) {
                return Err("cargo metadataのdependency packageが不足しています".into());
            }
            pending.push(dependency.clone());
        }
    }
    let local = reachable
        .iter()
        .filter_map(|id| packages.get(id).filter(|package| package.source.is_none()))
        .collect::<Vec<_>>();
    if local.is_empty() || local.len() > MAX_LOCAL_PACKAGES {
        return Err("local Cargo dependency closureが検証上限外です".into());
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
                    .map_err(|error| format!("Cargo configurationを確認できません: {error}"))?;
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
            .map_err(|error| format!("local dependency manifestを確認できません: {error}"))?;
        if canonical_manifest != manifest
            || canonical_manifest
                .file_name()
                .and_then(|name| name.to_str())
                != Some("Cargo.toml")
        {
            return Err("local dependency manifestのidentityが不正です".into());
        }
        let root = canonical_manifest
            .parent()
            .ok_or("local dependency rootがありません")?;
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
    .map_err(|_| "Cargo dependency graphをhash化できません")?;
    let project_graph_hash = polaris_core::conversation_state::content_hash(&graph_bytes);
    let source_bytes = serde_json::to_vec(&(
        "polaris.cargo-closure.source.v1",
        &project_graph_hash,
        &local_sources,
    ))
    .map_err(|_| "Cargo dependency sourceをhash化できません")?;
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
            return Err("CARGO_HOMEはabsolute pathである必要があります".into());
        }
        configured
    } else {
        let home = std::env::var_os("HOME").ok_or("Cargo homeを解決できません")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("HOMEはabsolute pathである必要があります".into());
        }
        home.join(".cargo")
    };
    directories.insert(if cargo_home.exists() {
        cargo_home
            .canonicalize()
            .map_err(|error| format!("Cargo homeを確認できません: {error}"))?
    } else {
        cargo_home
    });
    Ok(directories.into_iter().collect())
}

fn read_regular_resolution_input(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("{label}を確認できません: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!("{label}はregular fileである必要があります"));
    }
    std::fs::read(path).map_err(|error| format!("{label}を読めません: {error}"))
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
        .map_err(|error| format!("{label}を確認できません: {error}"))?;
    if canonical != path {
        return Err(format!("{label}のaliasは受け付けません"));
    }
    let bytes = read_regular_resolution_input(&canonical, label)?;
    Ok(hash_resolution_input(&canonical, &bytes, label))
}

fn bounded_tree_hash(root: &Path) -> Result<String, String> {
    const MAX_FILES: usize = 4096;
    const MAX_BYTES: u64 = 128 * 1024 * 1024;
    fn visit(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut entries = std::fs::read_dir(directory)
            .map_err(|error| format!("component treeを読めません: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("component treeを読めません: {error}"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "target" | ".git" | ".polaris") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("component treeを確認できません: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "component treeのsymlinkは検証できません: {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                visit(root, &path, files)?;
            } else if metadata.is_file() {
                files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
                if files.len() > MAX_FILES {
                    return Err("component treeのfile数が検証上限を超えました".into());
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
            .map_err(|error| format!("component sourceを読めません: {error}"))?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("component tree size overflow")?;
        if total > MAX_BYTES {
            return Err("component treeが検証上限を超えました".into());
        }
        records.push((
            relative.to_string_lossy().into_owned(),
            polaris_core::conversation_state::content_hash(&bytes),
        ));
    }
    let encoded = serde_json::to_vec(&records).map_err(|_| "component treeをhash化できません")?;
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
                "埋め込みURLとモデルは空でない値を両方指定し、--tool-memory historyまたはretrievalを有効にしてください",
            ),
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// 複数アカウントの登録・再認証・解除・表示名。
    Account(account::Args),
    /// 複数端末での共有：参加・状態・送受信・離脱。
    Sync(sync::Args),
    /// ローカルに保存した会話の検索・取得・削除。
    Memory(memory::Args),
    /// MCPサーバーの一覧・有効化・認証情報・カタログ確認・検出と登録。
    Mcp(mcp::Args),
    /// 同梱の定型コマンド：Next.jsとGitの検出・計画。
    Setting(setting::Args),
    /// フォルダ同士の類似を示す。名前とサイズだけを読む。
    Compare(compare::Args),
    /// 実装を実機で試してもらうための文面を作る。
    Handoff(handoff::Args),
    /// 余剰枠で頼まれていない整理を行ってよいかを判定する。
    Spare(spare::Args),
    /// このプロジェクトで決めたことを1文で残す。次の実行の前に、同じ
    /// 問いを蒸し返していないかの判定に使われる。
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
fn selected_model(explicit: Option<String>, environment: Option<String>) -> String {
    explicit
        .or(environment)
        .unwrap_or_else(|| polaris_provider::codex::DEFAULT_MODEL.to_string())
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
    polaris_tui::restore_prepared_continuation(
        session,
        &polaris_tui::PreparedContinuation {
            kind,
            persistence: saved.clone(),
        },
        config,
        workflow_mode.is_some(),
        skills,
        history_backend_factory,
    )
    .await
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
                    "--history-mode は保存済みセッションの方式と一致しません",
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
                    "--history-mode は保存済みセッションの方式と一致しません",
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

/// Fork/import only after provider, helper, and strict-history preconditions
/// have succeeded. Resume merely carries its earlier validated attachment.
fn prepare_continuation(
    resolved: ResolvedContinuation,
    data_root: &Path,
    database: &Path,
    project_id: &str,
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
                    "v2セッションが検証後に更新されました。再開し直してください",
                ));
            }
            match resolved.kind {
                polaris_tui::ContinuationKind::Resume => persistence,
                polaris_tui::ContinuationKind::Fork => persistence.fork("")?,
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

#[cfg(not(target_os = "linux"))]
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
                |error| format!("読取専用runtime imageを利用できません。修復が必要です: {error}"),
            )?,
        );
        polaris_core::strict_provider::managed_local_embedder(config, private_root, runtime)
            .map_err(|error| error.to_string())?
    };
    #[cfg(not(target_os = "macos"))]
    let mut embedder = {
        let _ = runtime_home;
        return Err("strict10の読取専用runtime imageはこの環境で利用できません".into());
    };
    embedder.attempt_ledger = Some(attempt_ledger);
    Ok((
        build_strict_history(summary, std::sync::Arc::new(embedder), decider),
        usage,
    ))
}
#[cfg(target_os = "linux")]
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
    Err("strict10の読取専用runtime imageはこの環境で利用できません".into())
}

#[cfg(target_os = "macos")]
fn managed_runtime_home() -> Result<PathBuf, String> {
    polaris_core::os_home::trusted_user_home()
        .map_err(|error| format!("管理runtimeのOSユーザホームを特定できません: {error}"))
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
        .ok_or_else(|| format!("--mcp-prompt は サーバー名/prompt名 の形で指定します: {spec}"))?;
    if server.is_empty() || prompt.is_empty() {
        return Err(format!(
            "--mcp-prompt にサーバー名かprompt名がありません: {spec}"
        ));
    }
    let mut map = serde_json::Map::new();
    for argument in arguments {
        let (key, value) = argument
            .split_once('=')
            .ok_or_else(|| format!("--mcp-prompt-arg は key=value の形で指定します: {argument}"))?;
        if key.is_empty() {
            return Err(format!("--mcp-prompt-arg にキーがありません: {argument}"));
        }
        if map
            .insert(key.to_owned(), serde_json::Value::String(value.to_owned()))
            .is_some()
        {
            return Err(format!("--mcp-prompt-arg のキーが重複しています: {key}"));
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
            "ビルド生成物を{}件（{:.1} GB）整理しました: {}",
            swept.removed,
            swept.freed_bytes as f64 / 1e9,
            directory.display()
        );
    }
    if swept.still_over() {
        // Saying nothing here would leave the person believing the cap holds.
        eprintln!(
            "ビルド生成物が上限を超えたままです: {} 。残りは新しいか、他から参照されています。",
            directory.display()
        );
    }
    if swept.skipped > 0 {
        eprintln!(
            "{}件は削除直前の再確認で見送りました。別のビルドが触れた可能性があります。",
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
    use polaris_core::config::DecisionBackend;

    if !config.enabled {
        return None;
    }
    let refuse = |reason: &str| -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
        eprintln!(
            "[decision] backend = \"{}\" を使えません（{reason}）。判定器なしで実行します",
            config.backend.as_str()
        );
        None
    };
    let decider: std::sync::Arc<dyn polaris_provider::decision::Decider> = match config.backend {
        DecisionBackend::SemIf => {
            // `available` re-checks the path here rather than trusting the
            // load-time check: the helper can be removed between the two.
            match polaris_provider::decision::semif::SemIfDecider::available(
                config.helper.as_deref(),
            ) {
                Some(decider) => std::sync::Arc::new(decider),
                None => return refuse("helperが実在する絶対pathではありません"),
            }
        }
        DecisionBackend::Jev => {
            let Ok(key) = std::env::var("POLARIS_JEV_API_KEY") else {
                return refuse("POLARIS_JEV_API_KEY がありません");
            };
            let base = config
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.typesafe.ai/v1".to_owned());
            let model = config
                .model
                .clone()
                .unwrap_or_else(|| "jev-latest".to_owned());
            match polaris_provider::decision::jev::JevDecider::new(&base, key, model) {
                Ok(decider) => std::sync::Arc::new(decider),
                Err(error) => return refuse(&format!("{error:?}")),
            }
        }
    };
    eprintln!("[decision] backend = \"{}\"", decider.name());
    Some(decider)
}

/// Final-answer observation has a separate one-shot disclosure boundary. The
/// normal Jev client may retry transient failures, so it cannot be reused for
/// this path.
fn build_finalizer_decider(
    config: &polaris_core::config::DecisionConfig,
) -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
    use polaris_core::config::DecisionBackend;

    if !config.enabled || config.backend != DecisionBackend::Jev {
        return None;
    }
    let key = std::env::var("POLARIS_JEV_API_KEY").ok()?;
    let base = config
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.typesafe.ai/v1".to_owned());
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "jev-latest".to_owned());
    polaris_provider::decision::jev::JevDecider::new_finalizer(&base, key, model)
        .ok()
        .map(|judge| {
            std::sync::Arc::new(judge) as std::sync::Arc<dyn polaris_provider::decision::Decider>
        })
}

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
    let (translator, unavailable) = if config.answer_finalizer.mode != AnswerFinalizerMode::Shadow
        || !translation_requested
    {
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
            resolved_home = polaris_core::os_home::trusted_user_home()
                .map_err(|error| format!("管理モデルのOSユーザホームを特定できません: {error}"))?;
            &resolved_home
        };
        match polaris_core::translation::ManagedCatTranslate::open(home) {
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
    polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner::open_configured(
        state_dir.join("answer-finalizer-shadow-v1.json"),
        config.presentation.clone(),
        config.answer_finalizer.clone(),
        decider,
        translator,
        unavailable,
    )
    .map_err(|error| error.to_string())
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
            .map_err(|error| format!("[tier.{tier}] のproviderを作れません: {error}"))?;
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
    let mut args = Args::parse();
    if args.web_search != WebSearchArg::Disabled {
        eprintln!(
            "Web検索は接続先の上限契約が未確認のため利用できません。disabledを使用してください。"
        );
        return ExitCode::FAILURE;
    }
    if let Err(error) = args.validate_tool_memory() {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    let tool_memory_embedding = args
        .tool_memory_embedding_url
        .clone()
        .zip(args.tool_memory_embedding_model.clone());

    match args.command.take() {
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
    let (_, preflight_sessions_dir) = match preflight_storage_paths(&cwd) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("Can't determine the storage directories: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut config = match polaris_core::config::load(&project_root) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("設定を読み込めません: {error}");
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
            eprintln!("プロジェクト識別子を取得できません: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(preflight_data_root) = preflight_sessions_dir.parent() else {
        eprintln!("v2セッションの保存先を特定できません");
        return ExitCode::FAILURE;
    };
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
            eprintln!("v2セッションを開けません: {error}");
            return ExitCode::FAILURE;
        }
    };
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
                "structured10は実験的な記憶モードのため、新しい会話では --history-mode structured10 の明示指定が必要です"
            );
            return ExitCode::FAILURE;
        }
        eprintln!(
            "structured10は実験的な記憶モードです。実モデルでの評価前のため既定にはしていません。"
        );
    }

    let model = selected_model(args.model.clone(), std::env::var("POLARIS_MODEL").ok());
    let mut provider_name = std::env::var("POLARIS_PROVIDER").unwrap_or_else(|_| {
        let has_openai_key = std::env::var("POLARIS_API_KEY").is_ok()
            || polaris_auth::api_key::default_path()
                .ok()
                .and_then(|p| polaris_auth::api_key::load_from(&p).ok().flatten())
                .is_some();
        let has_saved_codex_credentials = polaris_auth::store::default_path()
            .ok()
            .map(|p| p.exists())
            .unwrap_or(false);
        default_provider_name(has_openai_key, has_saved_codex_credentials).to_string()
    });

    let model_name: String;
    // A supplied `--effort` is fixed in both modes. One-shot runs retain the
    // historical xhigh default; an interactive TUI leaves this absent so Jev
    // can decide every turn and the footer can begin at `undecided`.
    let initial_effort_name =
        selected_effort(args.effort.as_deref(), args.prompt.is_some()).map(str::to_owned);
    let attempt_parent_id = match polaris_core::session_store::new_session_id() {
        Ok(id) => id,
        Err(error) => {
            eprintln!("試行記録の親IDを作成できません: {error}");
            return ExitCode::FAILURE;
        }
    };
    let attempt_ledger = polaris_provider::attempts::AttemptLedger::with_context(
        polaris_provider::attempts::AttemptContext {
            parent_id: Some(attempt_parent_id.clone()),
            kind: polaris_provider::attempts::AttemptKind::Completion,
        },
    );

    // Set by the `llamacpp` arm. Never read again on purpose: holding it
    // is the whole job. `LlamaServer` kills the child when it drops, so
    // the binding has to outlive every path that uses the provider — and
    // it drops at the end of `main`, including on the early returns.
    let mut _llama_server: Option<polaris_core::llama_server::LlamaServer> = None;
    let (provider, mcp_selection_provider, tier_weights): (Shared, Option<Shared>, TierWeights) = loop {
        match provider_name.as_str() {
            "openai" => {
                // Keep the existing setup as-is. Behavior does not change.
                let base = std::env::var("POLARIS_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
                let key = std::env::var("POLARIS_API_KEY").ok().or_else(|| {
                    polaris_auth::api_key::default_path().ok().and_then(|p| {
                        match polaris_auth::api_key::load_from(&p) {
                            Ok(k) => k,
                            Err(e) => {
                                eprintln!("Can't read the saved API key: {e}");
                                None
                            }
                        }
                    })
                });
                let key = match key {
                    Some(k) => k,
                    None if args.prompt.is_none() => {
                        let auth_store_path = match polaris_auth::store::default_path() {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("Can't determine where to store credentials: {e}");
                                return ExitCode::FAILURE;
                            }
                        };
                        let api_key_path = match polaris_auth::api_key::default_path() {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("Can't determine where to store the API key: {e}");
                                return ExitCode::FAILURE;
                            }
                        };
                        match polaris_tui::onboarding::run(&auth_store_path, &api_key_path).await {
                            Ok(polaris_tui::onboarding::Outcome::ApiKeySaved) => continue,
                            Ok(polaris_tui::onboarding::Outcome::CodexLoggedIn) => {
                                provider_name = "codex".to_string();
                                continue;
                            }
                            Err(polaris_tui::onboarding::OnboardingError::Cancelled) => {
                                return ExitCode::SUCCESS;
                            }
                            Err(e) => {
                                eprintln!("{e}");
                                return ExitCode::FAILURE;
                            }
                        }
                    }
                    None => {
                        eprintln!("POLARIS_API_KEY is not set");
                        return ExitCode::FAILURE;
                    }
                };
                if args.account.is_some() {
                    eprintln!(
                        "--accountはcodexのみで使えます。POLARIS_PROVIDER=codexを指定してください。"
                    );
                    return ExitCode::FAILURE;
                }
                let model = model.clone();
                model_name = model.clone();
                let selector = OpenAiProvider::new(
                    base.clone(),
                    key.clone(),
                    polaris_core::mcp::preselection::MODEL.into(),
                )
                .ok()
                .map(|provider| {
                    std::sync::Arc::new(provider.with_attempt_ledger(attempt_ledger.clone()))
                        as std::sync::Arc<dyn polaris_provider::Provider>
                });
                let weights = tier_providers(
                    &config.tiers,
                    &model,
                    initial_effort_name.as_deref(),
                    |named| {
                        OpenAiProvider::new(base.clone(), key.clone(), named.to_owned())
                            .map(|provider| {
                                std::sync::Arc::new(
                                    provider.with_attempt_ledger(attempt_ledger.clone()),
                                )
                                    as std::sync::Arc<dyn polaris_provider::Provider>
                            })
                            .map_err(|error| error.to_string())
                    },
                );
                let weights = match weights {
                    Ok(weights) => weights,
                    Err(message) => {
                        eprintln!("{message}");
                        return ExitCode::FAILURE;
                    }
                };
                match OpenAiProvider::new(base, key, model)
                    .map(|provider| provider.with_attempt_ledger(attempt_ledger.clone()))
                {
                    Ok(p) => break (std::sync::Arc::new(p), selector, weights),
                    Err(e) => {
                        eprintln!("Can't build the client: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "codex" => {
                let store = match polaris_auth::store::default_path() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Can't determine where to store credentials: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                let model = model.clone();
                model_name = model.clone();
                // 登録が1件でもあれば、このrunは1つの登録へ固定される。
                if let Err(message) = account::bind(&model, args.account.as_deref()) {
                    eprintln!("{message}");
                    return ExitCode::FAILURE;
                }
                let tokens = match account::require(store) {
                    Ok(tokens) => tokens,
                    Err(message) => {
                        eprintln!("{message}");
                        return ExitCode::FAILURE;
                    }
                };
                let selector = polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.to_string(),
                    polaris_core::mcp::preselection::MODEL.into(),
                    tokens.clone(),
                )
                .ok()
                .map(|provider| {
                    let provider = provider.with_attempt_ledger(attempt_ledger.clone());
                    // This picks which of nine catalogue entries to offer
                    // and nothing else, and it runs at whatever effort the
                    // account's plan implies — `xhigh` on a pro plan, which
                    // spent 605.7 of the 649.3 tokens it produced per run on
                    // reasoning, most often to arrive at `{"skills":[]}`.
                    //
                    // Setting it to `low` does cut that: output fell 29%,
                    // from 1,298 tokens a run to 926. It also cost more,
                    // $0.1371 against $0.1322 over n=12 each, because the
                    // run took 0.9 more astra requests and one of those is
                    // worth $0.013. A cheaper selector that picks worse
                    // skills is not cheaper. Measured 2026-09-21; not
                    // repeated without separating it from the block change
                    // it was measured beside.
                    std::sync::Arc::new(provider) as std::sync::Arc<dyn polaris_provider::Provider>
                });
                let weights = tier_providers(
                    &config.tiers,
                    &model,
                    initial_effort_name.as_deref(),
                    |named| {
                        polaris_provider::codex::CodexProvider::new(
                            polaris_provider::codex::ENDPOINT_BASE.to_string(),
                            named.to_owned(),
                            tokens.clone(),
                        )
                        .map(|provider| {
                            std::sync::Arc::new(
                                provider.with_attempt_ledger(attempt_ledger.clone()),
                            )
                                as std::sync::Arc<dyn polaris_provider::Provider>
                        })
                        .map_err(|error| error.to_string())
                    },
                );
                let weights = match weights {
                    Ok(weights) => weights,
                    Err(message) => {
                        eprintln!("{message}");
                        return ExitCode::FAILURE;
                    }
                };
                match polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.to_string(),
                    model,
                    tokens,
                )
                .map(|provider| provider.with_attempt_ledger(attempt_ledger.clone()))
                {
                    Ok(provider) => break (std::sync::Arc::new(provider), selector, weights),
                    Err(error) => {
                        eprintln!("Can't build the client: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            // The bundled `llama-server`, started by this process. The
            // other two local runtimes (Ollama, LM Studio) are reachable
            // only from the desktop's role bindings; this arm exists so
            // the CLI — which is what the measurement harnesses drive —
            // can run a local model at all.
            "llamacpp" => {
                let Some(llama) = config.llama_cpp.clone() else {
                    eprintln!(
                        "POLARIS_PROVIDER=llamacpp には `[llama_cpp]` の server・models_dir・build が要ります。"
                    );
                    return ExitCode::FAILURE;
                };
                let state = match polaris_core::project::state_dir(&project_root) {
                    Ok(directory) => directory,
                    Err(error) => {
                        eprintln!("状態ディレクトリを特定できません: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                let started = async {
                    let server =
                        polaris_core::llama_server::LlamaServer::start(&llama, &state).await?;
                    let adapter = polaris_core::llama_server::adapter(&server)
                        .map_err(|_| std::io::Error::other("問い合わせ口を作成できません"))?;
                    let inventory = adapter
                        .inventory_with_load_state()
                        .await
                        .map_err(|error| std::io::Error::other(format!("{error}")))?;
                    let observed = inventory
                        .iter()
                        .find(|held| held.id() == model)
                        .ok_or_else(|| {
                            let names: Vec<&str> = inventory.iter().map(|held| held.id()).collect();
                            std::io::Error::other(format!(
                                "{model} が {} にありません。あるのは {names:?}",
                                llama.models_dir.display()
                            ))
                        })?;
                    let selection = adapter
                        .select(observed)
                        .await
                        .map_err(|error| std::io::Error::other(format!("{error}")))?;
                    let provider =
                        polaris_provider::local_inference::LocalInferenceProvider::with_token(
                            selection,
                            server.token(),
                        )
                        .map_err(|error| std::io::Error::other(format!("{error}")))?;
                    Ok::<_, std::io::Error>((server, provider))
                }
                .await;
                let (server, provider) = match started {
                    Ok(started) => started,
                    Err(error) => {
                        eprintln!("llama-serverを起動できません: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                // Held for the life of the process: dropping it kills the
                // child, and an orphan would hold the port and the model.
                _llama_server = Some(server);
                model_name = model.clone();
                break (std::sync::Arc::new(provider), None, TierWeights::new());
            }
            other => {
                eprintln!(
                    "POLARIS_PROVIDER is an unknown value {other}. Specify openai, codex or llamacpp"
                );
                return ExitCode::FAILURE;
            }
        }
    };

    if let Some(effort) = initial_effort_name.as_deref() {
        provider.set_effort(Some(effort));
    }

    // Wrapped before anything takes a handle, so every consultation is
    // counted whichever path reaches the judge. The judge bills separately
    // from the models and at a different rate; leaving it out made a run
    // with the judge on report its cost with the judge's part missing.
    let decision_meter = polaris_provider::decision::DecisionMeter::default();
    let decider = build_decider(&config.decision).map(|decider| decision_meter.wrap(decider));
    // The same judge weighs delegated tasks. One registration, because both
    // ask the same service the same kind of question.
    let tier_decider: Option<std::sync::Arc<dyn polaris_core::tier::TierDecider>> =
        decider.clone().map(|decider| {
            std::sync::Arc::new(polaris_core::tier::typed::TypedTierDecider::new(decider))
                as std::sync::Arc<dyn polaris_core::tier::TierDecider>
        });

    // A judge that routes delegations by weight, over four weights that
    // all resolve to the same provider, changes nothing and says nothing.
    // `build_tier_providers` skips a weight that names neither a model nor
    // an effort, so an installation with no `[tier.*]` gets one provider
    // wearing four names — and the ladder's whole point is that they
    // differ. Say it once; do not invent model names as defaults, because
    // which model each weight buys is the project's decision and a wrong
    // guess here spends its money.
    if tier_decider.is_some() && config.tiers.is_empty() {
        eprintln!(
            "警告: 判定器は委譲を4段へ振り分けますが、`[tier.*]` に model も effort も書かれていないため、4段すべてが同じproviderへ解決されます。段ごとの設定を書くまで、振り分けは費用にも能力にも効きません。"
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
                eprintln!("回答達成判定を準備できません: {error}");
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
        eprintln!("v2セッションの保存先を特定できません");
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
                eprintln!("--routine-authorityには有効なworkflowが必要です");
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
                    eprintln!("routine authorityを準備できません: {error}");
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
                eprintln!("review authorityを準備できません: {error}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    // The binary that gets re-executed is staged outside the writable root.
    // Skipping this would let anyone who can write to the workspace replace
    // the helper (see the docs on `polaris_sandbox::stage`).
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
                "警告: {} のalways-on節は{}行のうち{}行しかモデルへ届きません（{}行は上限で切り捨て、届く分は{}トークン）。内訳は `polaris doctor` にあります。",
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
            eprintln!("AGENTS.mdの読取元を準備できません: {error}");
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
                    "strict10 には [embedding] の model_path、revision と検証済み読取専用runtime imageが必要です".into(),
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
                eprintln!("履歴を準備できません: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let continuation = match resolved_continuation {
        Some(resolved) => prepare_continuation(
            resolved,
            data_root,
            &state_dir.join("memory.sqlite3"),
            &project_id,
        )
        .map(Some),
        None => Ok(None),
    };
    let continuation = match continuation {
        Ok(continuation) => continuation,
        Err(error) => {
            eprintln!("v2セッションを準備できません: {error}");
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
            eprintln!("AGENTS.mdを読み込めません: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(not(unix))]
    let always_on = prompt::assemble_always_on(&constitution, &environment, &discovered.skills);
    let always_on = match always_on.with_project_root(&project_root) {
        Ok(prompt) => prompt,
        Err(error) => {
            eprintln!("プロジェクトの参照先を設定できません: {error}");
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
                    .map_err(|_| "routine authority stateを取得できません".to_owned())?
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
                        .map_err(|_| "routine authority stateを復元できません".to_owned())?;
                    *slot = Some(plan);
                }
                result
            }) as polaris_tui::TurnAuthorityInstaller
        });

    match args.prompt.clone() {
        Some(prompt) => {
            let usage_meter = polaris_provider::UsageMeter::default();
            let provider: std::sync::Arc<dyn polaris_provider::Provider> =
                std::sync::Arc::new(usage_meter.wrap(provider.clone()));
            let mcp_selection_provider = mcp_selection_provider.map(|provider| {
                std::sync::Arc::new(usage_meter.wrap(provider))
                    as std::sync::Arc<dyn polaris_provider::Provider>
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
                            eprintln!("ワークフローを開始できません: {error}");
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
                    eprintln!("--phase には設定で [workflow].enabled = true が必要です");
                    return ExitCode::FAILURE;
                };
                if let Err(error) = workflow.request_phase(phase) {
                    eprintln!("ワークフロー段階を設定できません: {error}");
                    return ExitCode::FAILURE;
                }
            }
            let project_id = match polaris_tui::persist::project_identity(&project_root) {
                Ok(id) => id,
                Err(error) => {
                    eprintln!("プロジェクト識別子を取得できません: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let data_root = match sessions_dir.parent() {
                Some(root) => root,
                None => {
                    eprintln!("v2セッションの保存先を特定できません");
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
                (None, None)
                    if config.workflow.enabled
                        || history_mode
                            != polaris_core::conversation_state::HistoryMode::Legacy =>
                {
                    Some(polaris_core::session_store::PersistedSession::create(
                        data_root,
                        &database,
                        &project_id,
                        history_mode,
                        session.workflow.as_ref(),
                    ))
                }
                (None, None) => None,
                _ => unreachable!("clap rejects mutually exclusive session flags"),
            };
            let persistence = match persistence {
                None => None,
                Some(Ok(persistence)) => Some(persistence),
                Some(Err(error)) => {
                    eprintln!("v2セッションを開けません: {error}");
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
            if args.resume.is_none()
                && args.fork.is_none()
                && let Some(persistence) = persistence
                && let Err(error) = session.attach(persistence)
            {
                eprintln!("v2セッションを接続できません: {error}");
                return ExitCode::FAILURE;
            }
            if let Err(error) = session.restore_verification_checkpoint() {
                eprintln!("verification checkpointを復元できません: {error}");
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
                        eprintln!("routine authorityを接続できません: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                if let Some(review) = prepared_review_authority.clone()
                    && let Err(error) = apply_review_authority(&mut session, &owner, review)
                {
                    eprintln!("review authorityを適用できません: {error}");
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
                eprintln!("strict10履歴を復元できません: {error}");
                if let Some(usage) = &session.summary_usage {
                    let usage = usage.snapshot();
                    eprintln!(
                        "要約の確認済み消費: 入力 {} / 出力 {} / 合計 {}。欠測 {} / 失敗 {}。",
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
                        eprintln!("履歴保存を準備できません: {error}");
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
                eprintln!("圧縮前の履歴を退避できません: {error}");
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
                eprintln!("ツール記憶を準備できません: {error}");
                return ExitCode::FAILURE;
            }
            if args.tool_memory.retention().is_some() {
                eprintln!("ツール記憶セッション: {session_id}");
            }
            session.push_user(&prompt);
            let ceiling =
                polaris_core::mcp::operation::ExternalCeiling::host_external(args.mcp_external);
            let mut mcp_runtime = None;
            let mut mcp_revision = None;
            match polaris_core::mcp::local::LocalHost::open(&cwd, false) {
                Ok(host) => {
                    mcp_revision = Some(registry_revision(&host));
                    match host.runtime(&session_id, "exec", ceiling) {
                        Ok(Some((mut runtime, quarantined))) => {
                            if !quarantined.is_empty() && host.root_contains_home() {
                                eprintln!(
                                    "作業フォルダがhome全体として扱われるため、コマンドで起動するMCPサーバーは使えません。.gitのあるフォルダなどで実行してください。"
                                );
                            }
                            for (server, reason) in quarantined {
                                eprintln!("MCPサーバー {server} は使えません: {reason}");
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
                                        eprintln!("{message}");
                                        return ExitCode::FAILURE;
                                    }
                                };
                                match runtime.host_prompt(&choice, std::future::pending()).await {
                                    Ok(result) if result.permits_continuation() => {
                                        eprintln!(
                                            "MCP prompt {}/{} を選びました",
                                            choice.server, choice.prompt
                                        );
                                    }
                                    Ok(_) => {
                                        eprintln!(
                                            "MCP prompt {}/{} は確認できませんでした",
                                            choice.server, choice.prompt
                                        );
                                        return ExitCode::FAILURE;
                                    }
                                    Err(error) => {
                                        eprintln!("MCP promptを選べません: {error}");
                                        return ExitCode::FAILURE;
                                    }
                                }
                            }
                            mcp_runtime = Some(runtime);
                        }
                        Ok(None) => {}
                        Err(error) => eprintln!("MCPサーバーを準備できません: {error}"),
                    }
                    session.mcp_setup = Some(std::sync::Arc::new(host.setup_tool(ceiling)));
                }
                Err(error) => eprintln!("MCPを使わずに実行します: {error}"),
            }
            // 同梱の定型コマンド。projectのrootへ束縛するので、実行した
            // directoryではなくprojectが対象になる。
            session.setting = Some(std::sync::Arc::new(
                polaris_core::recipe::tool::SettingTool::new(polaris_core::project::resolve_root(
                    &cwd,
                )),
            ));
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
            let durable_session_id = session
                .persistence
                .as_ref()
                .and_then(|store| store.snapshot().ok())
                .map(|snapshot| snapshot.state.session_id)
                .unwrap_or_else(|| session_id.clone());
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
                    eprintln!("subagent controlを準備できません: {error}");
                    return ExitCode::FAILURE;
                }
            }

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
            // Child MCP read snapshots are in-memory, root-bound evidence.
            // Do not let a one-shot CLI root retain them while shutdown,
            // output formatting, or later process reuse runs.
            let _ = session.revoke_root_mcp_leases().await;
            if let Some(runtime) = mcp_runtime.as_mut()
                && runtime.shutdown().await.is_err()
            {
                eprintln!("MCPサーバーの停止を確認できません。");
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
                        "計測器の総量: in {} / out {} / cache {} / total {}",
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
                    let judged = decision_meter.snapshot();
                    if judged.reported + judged.missing + judged.failed > 0 {
                        eprintln!(
                            "  判定器: in {} / out {} — 応答あり {} / 使用量なし {} / 失敗 {}",
                            judged.input_tokens,
                            judged.output_tokens,
                            judged.reported,
                            judged.missing,
                            judged.failed,
                        );
                    }
                    let coverage = usage_meter.snapshot();
                    eprintln!(
                        "使用量の計測: 応答あり {} / 欠測 {} / 失敗 {}。欠測・失敗分の消費は不明です。",
                        coverage.reported_responses,
                        coverage.missing_responses,
                        coverage.failed_requests
                    );
                    if let Some(usage) = &session.summary_usage {
                        let usage = usage.snapshot();
                        eprintln!(
                            "要約の確認済み消費: 入力 {} / 出力 {} / 合計 {}。欠測 {} / 失敗 {}。",
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
                        "終了までの確認済み消費: 入力 {} / 出力 {} / 合計 {}。欠測 {} / 失敗 {}。",
                        coverage.usage.input_tokens,
                        coverage.usage.output_tokens,
                        coverage.usage.total_tokens,
                        coverage.missing_responses,
                        coverage.failed_requests
                    );
                    if let Some(usage) = &session.summary_usage {
                        let usage = usage.snapshot();
                        eprintln!(
                            "要約の確認済み消費: 入力 {} / 出力 {} / 合計 {}。欠測 {} / 失敗 {}。",
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
                provider: provider.clone(),
                mcp_selection_provider,
                decider: decider.clone(),
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
            eprintln!("計画を読めません: {error}");
            return ExitCode::FAILURE;
        }
    };
    let plan: polaris_core::common_tests::Plan = match serde_json::from_str(&body) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("計画の形式が不正です: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(refused) = plan.check() {
        eprintln!("この計画は受理できません: {refused:?}");
        return ExitCode::FAILURE;
    }
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("実行directoryを確定できません: {error}");
            return ExitCode::FAILURE;
        }
    };
    let write_root = polaris_core::project::resolve_root(&cwd);
    let policy = match build_sandbox_policy(&cwd, sandbox_mode) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("sandboxを準備できません: {error}");
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
            eprintln!("共通case結果を表せません: {error}");
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
        "共通caseの子プロセスを回収しています{}。完了するまで終了しません。",
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
                        "共通caseの回収を再試行しています ({}秒経過): {error}",
                        started.elapsed().as_secs()
                    );
                    last_notice = std::time::Instant::now();
                }
            }
        }
        if last_notice.elapsed() >= std::time::Duration::from_secs(5) {
            eprintln!(
                "共通caseの回収を待機しています ({}秒経過)。子プロセスと作業領域は保持中です。",
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
            "計画は {} bytes を超えられません",
            polaris_core::common_tests::MAX_PLAN_BYTES
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("UTF-8ではありません: {error}"))
}

/// `polaris doctor`. Diagnoses the local installation: read-only checks
/// only, and never prints the credential contents themselves — only
/// whether each file exists and parses.
async fn run_doctor() -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    println!("polaris doctor\n");

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

    println!("\nState:");
    match polaris_core::project::state_dir(&cwd) {
        Ok(d) => println!("  state directory: {}", d.display()),
        Err(e) => println!("  Can't determine the state directory: {e}"),
    }

    // Which project rules actually reach the model. The ceiling truncates
    // from the end in silence, and a global AGENTS.md halves the project's
    // share, so "I wrote the rule down" and "the model was told" are two
    // different facts. This prints the second one.
    println!("\nAGENTS.md (always-on):");
    let project_root = polaris_core::project::resolve_root(&cwd);
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

/// When `POLARIS_PROVIDER` isn't set, decides which provider arm to try
/// first. Prefers `openai` if a key is available (env var or the saved
/// `api_key.json`), since that's the arm onboarding itself lives on. If no
/// openai key exists but the user already signed in with ChatGPT (saved
/// `~/.polaris/auth.json`, from `polaris login` or onboarding), prefer
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
mod prompt_choice_tests {
    use super::mcp_prompt_choice;

    #[test]
    fn a_choice_names_one_server_and_one_prompt() {
        let held = mcp_prompt_choice("files/review", &["path=src/lib.rs".into(), "深さ=2".into()])
            .expect("a well-formed choice");
        assert_eq!(held.server, "files");
        assert_eq!(held.prompt, "review");
        assert_eq!(held.arguments["path"], "src/lib.rs");
        // Values stay strings. Parsing them as JSON would make an argument
        // change type depending on what it looked like, and the server
        // would be told something other than what was typed.
        assert_eq!(held.arguments["深さ"], "2");
        assert!(held.arguments["深さ"].is_string());
    }

    #[test]
    fn a_malformed_choice_is_refused_before_anything_connects() {
        for refused in ["files", "/review", "files/", ""] {
            assert!(
                mcp_prompt_choice(refused, &[]).is_err(),
                "{refused:?} does not name a server and a prompt"
            );
        }
        for refused in ["nokey", "=value"] {
            assert!(
                mcp_prompt_choice("files/review", &[refused.into()]).is_err(),
                "{refused:?} is not key=value"
            );
        }
        // A repeated key would silently keep one of the two values.
        assert!(
            mcp_prompt_choice("files/review", &["a=1".into(), "a=2".into()]).is_err(),
            "a duplicate key has no obvious winner"
        );
        // A value may contain `=`; only the first one separates.
        let held = mcp_prompt_choice("files/review", &["q=a=b".into()]).expect("one separator");
        assert_eq!(held.arguments["q"], "a=b");
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn resume_and_fork_restore_saved_focus_without_resetting_runtime_settings() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{Phase, SessionWorkflow, WorkflowConfig},
        };
        let dir = tempfile::tempdir().unwrap();
        let source_config = WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Review,
            mode: polaris_core::workflow::WorkflowMode::Adaptive,
            ..WorkflowConfig::default()
        };
        let source_workflow = SessionWorkflow::new(source_config);
        let source = PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "test-project",
            HistoryMode::Legacy,
            Some(&source_workflow),
        )
        .unwrap();
        let child = source.fork(&"a".repeat(64)).unwrap();
        let current_config = WorkflowConfig {
            initial_phase: Phase::Implement,
            ..polaris_core::config::Config::default().workflow
        };
        for (flag, saved) in [("--resume", &source), ("--fork", &child)] {
            let id = saved.snapshot().unwrap().state.session_id;
            let args = Args::try_parse_from(["polaris", flag, &id]).unwrap();
            let mut session = Session {
                workflow: Some(SessionWorkflow::new(current_config.clone())),
                disable_files_md_auto_regenerate: true,
                ..Session::default()
            };
            restore_continuation_workflow(
                args.resume.as_deref(),
                args.fork.as_deref(),
                &mut session,
                saved,
                &current_config,
                None,
                &[],
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                session.workflow.as_ref().unwrap().state().phase,
                Phase::Review,
                "{flag}"
            );
            assert_eq!(
                session.workflow.as_ref().unwrap().config,
                source_workflow.config
            );
            assert!(session.disable_files_md_auto_regenerate);
            let disabled = WorkflowConfig::default();
            restore_continuation_workflow(
                args.resume.as_deref(),
                args.fork.as_deref(),
                &mut session,
                saved,
                &disabled,
                None,
                &[],
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                session.workflow.as_ref().unwrap().config,
                source_workflow.config
            );

            restore_continuation_workflow(
                args.resume.as_deref(),
                args.fork.as_deref(),
                &mut session,
                saved,
                &disabled,
                Some(WorkflowModeArg::Off),
                &[],
                None,
            )
            .await
            .unwrap();
            assert!(!session.workflow.as_ref().unwrap().config.enabled);
            assert!(
                !saved
                    .snapshot()
                    .unwrap()
                    .state
                    .workflow_config
                    .unwrap()
                    .config
                    .enabled
            );
        }
    }

    #[test]
    fn prepared_continuation_resolves_resume_and_fork_without_fresh_fallback() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{Phase, SessionWorkflow, WorkflowConfig},
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("memory.sqlite3");
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Review,
            ..Default::default()
        });
        let source = PersistedSession::create(
            dir.path(),
            &database,
            "project",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .expect("create source");
        let source_id = source.snapshot().expect("source snapshot").state.session_id;

        let resumed = resolve_continuation(
            Some(&source_id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .expect("resolve resume")
        .expect("resume target");
        let resumed = prepare_continuation(resumed, dir.path(), &database, "project")
            .expect("prepare resume");
        assert_eq!(resumed.kind, polaris_tui::ContinuationKind::Resume);
        assert_eq!(
            resumed.persistence.snapshot().unwrap().state.session_id,
            source_id
        );

        let before_parent = source.snapshot().expect("parent before fork");
        let forked = resolve_continuation(
            None,
            Some(&source_id),
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .expect("resolve fork")
        .expect("fork target");
        let forked =
            prepare_continuation(forked, dir.path(), &database, "project").expect("prepare fork");
        assert_eq!(forked.kind, polaris_tui::ContinuationKind::Fork);
        assert_ne!(
            forked.persistence.snapshot().unwrap().state.session_id,
            source_id
        );
        let after_parent = source.snapshot().expect("parent after fork");
        assert_eq!(after_parent.state, before_parent.state);
        assert_eq!(after_parent.raw_hash, before_parent.raw_hash);
        assert_eq!(after_parent.raw_offset, before_parent.raw_offset);
        assert_eq!(after_parent.events.len(), before_parent.events.len());
    }

    #[test]
    fn missing_v2_target_can_import_a_valid_legacy_session() {
        use polaris_core::conversation_state::HistoryMode;
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("memory.sqlite3");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions).expect("legacy sessions directory");
        let id = "00000000-0000-4000-8000-000000000001";
        polaris_tui::persist::append_message(
            &sessions.join(format!("{id}.jsonl")),
            &polaris_provider::Message::user("legacy turn"),
        )
        .expect("legacy fixture");

        let resolved = resolve_continuation(
            Some(id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &sessions,
        )
        .expect("resolve legacy")
        .expect("legacy target");
        let prepared = prepare_continuation(resolved, dir.path(), &database, "project")
            .expect("import legacy");
        assert_eq!(prepared.kind, polaris_tui::ContinuationKind::Resume);
        assert_eq!(
            prepared.persistence.snapshot().unwrap().events[0]
                .message
                .content,
            "legacy turn"
        );
    }

    #[test]
    fn resolver_rejects_corrupt_legacy_and_incomplete_v2_without_repairing() {
        use polaris_core::conversation_state::{
            ConversationStateV2, ConversationStore, HistoryMode,
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("memory.sqlite3");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir(&sessions).expect("legacy sessions directory");
        let corrupt_id = "00000000-0000-4000-8000-000000000001";
        let corrupt = sessions.join(format!("{corrupt_id}.jsonl"));
        std::fs::write(&corrupt, b"{not json}\n").expect("corrupt fixture");
        let corrupt_before = std::fs::read(&corrupt).unwrap();
        assert!(
            resolve_continuation(
                Some(corrupt_id),
                None,
                dir.path(),
                "project",
                None,
                HistoryMode::Legacy,
                &sessions,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&corrupt).unwrap(), corrupt_before);
        assert!(!database.exists());

        let id = "00000000-0000-4000-8000-000000000002";
        let state = ConversationStateV2::new("project".into(), id.into()).expect("state");
        let store = ConversationStore::create(dir.path(), state).expect("v2 fixture");
        std::fs::remove_file(store.directory().join("writer.lock")).expect("remove lock fixture");
        let colliding = sessions.join(format!("{id}.jsonl"));
        polaris_tui::persist::append_message(
            &colliding,
            &polaris_provider::Message::user("must not import"),
        )
        .expect("collision fixture");
        let state_before = std::fs::read(store.directory().join("state.json")).unwrap();
        let raw_before = std::fs::read(store.directory().join("raw.jsonl")).unwrap();
        let colliding_before = std::fs::read(&colliding).unwrap();
        assert!(
            resolve_continuation(
                Some(id),
                None,
                dir.path(),
                "project",
                None,
                HistoryMode::Legacy,
                &sessions,
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(store.directory().join("state.json")).unwrap(),
            state_before
        );
        assert_eq!(
            std::fs::read(store.directory().join("raw.jsonl")).unwrap(),
            raw_before
        );
        assert_eq!(std::fs::read(&colliding).unwrap(), colliding_before);
        assert!(!database.exists());
    }

    #[test]
    fn resolver_does_not_open_missing_or_old_database() {
        use polaris_core::conversation_state::{
            ConversationStateV2, ConversationStore, HistoryMode,
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("memory.sqlite3");
        let id = "00000000-0000-4000-8000-000000000003";
        let state = ConversationStateV2::new("project".into(), id.into()).expect("state");
        ConversationStore::create(dir.path(), state).expect("v2 fixture");

        let missing = resolve_continuation(
            Some(id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .expect("resolve without database")
        .expect("target");
        assert_eq!(missing.history_mode, HistoryMode::Legacy);
        assert!(!database.exists());

        std::fs::write(&database, b"not a sqlite database").expect("old database fixture");
        let before = std::fs::read(&database).unwrap();
        assert!(
            resolve_continuation(
                Some(id),
                None,
                dir.path(),
                "project",
                None,
                HistoryMode::Legacy,
                &dir.path().join("sessions"),
            )
            .is_ok()
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn rejected_continuation_targets_do_not_create_a_fresh_session() {
        use polaris_core::{
            conversation_state::{ConversationStateV2, ConversationStore, HistoryMode},
            session_store::PersistedSession,
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("memory.sqlite3");
        let source = PersistedSession::create(
            dir.path(),
            &database,
            "other-project",
            HistoryMode::Legacy,
            None,
        )
        .expect("create foreign source");
        let foreign_id = source.snapshot().unwrap().state.session_id;
        let foreign_dir = dir.path().join("sessions-v2").join(&foreign_id);
        let foreign_state_before = std::fs::read(foreign_dir.join("state.json")).expect("state");
        let foreign_raw_before = std::fs::read(foreign_dir.join("raw.jsonl")).expect("raw");
        let legacy_dir = dir.path().join("sessions");
        std::fs::create_dir(&legacy_dir).expect("legacy sessions directory");
        let colliding_legacy = legacy_dir.join(format!("{foreign_id}.jsonl"));
        polaris_tui::persist::append_message(
            &colliding_legacy,
            &polaris_provider::Message::user("legacy collision"),
        )
        .expect("legacy collision fixture");
        let legacy_before = std::fs::read(&colliding_legacy).expect("legacy bytes");
        let allowed_id = "b".repeat(128);
        let allowed_legacy = legacy_dir.join(format!("{allowed_id}.jsonl"));
        polaris_tui::persist::append_message(
            &allowed_legacy,
            &polaris_provider::Message::user("allowed legacy"),
        )
        .expect("128-byte legacy fixture");
        let allowed_before = std::fs::read(&allowed_legacy).expect("allowed bytes");
        let rejected_id = "a".repeat(129);
        let rejected_legacy = legacy_dir.join(format!("{rejected_id}.jsonl"));
        polaris_tui::persist::append_message(
            &rejected_legacy,
            &polaris_provider::Message::user("rejected legacy"),
        )
        .expect("129-byte legacy fixture");
        let rejected_before = std::fs::read(&rejected_legacy).expect("rejected bytes");
        let traversal_legacy = dir.path().join("traversal.jsonl");
        polaris_tui::persist::append_message(
            &traversal_legacy,
            &polaris_provider::Message::user("traversal legacy"),
        )
        .expect("traversal legacy fixture");
        let traversal_before = std::fs::read(&traversal_legacy).expect("traversal bytes");
        let bytes_before = std::fs::read(&database).expect("database bytes");
        let sessions_before = std::fs::read_dir(dir.path()).expect("data root").count();
        let v2_before = std::fs::read_dir(dir.path().join("sessions-v2"))
            .expect("v2 sessions")
            .count();
        assert!(
            resolve_continuation(
                Some(&allowed_id),
                None,
                dir.path(),
                "project",
                None,
                HistoryMode::Legacy,
                &legacy_dir,
            )
            .is_ok()
        );
        for id in [
            "missing".to_string(),
            foreign_id,
            "../traversal".to_string(),
            rejected_id,
        ] {
            assert!(
                resolve_continuation(
                    Some(&id),
                    None,
                    dir.path(),
                    "project",
                    None,
                    HistoryMode::Legacy,
                    &dir.path().join("sessions"),
                )
                .is_err()
            );
        }
        assert_eq!(std::fs::read(&database).unwrap(), bytes_before);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            sessions_before
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("sessions-v2"))
                .unwrap()
                .count(),
            v2_before
        );
        assert_eq!(
            std::fs::read(foreign_dir.join("state.json")).unwrap(),
            foreign_state_before
        );
        assert_eq!(
            std::fs::read(foreign_dir.join("raw.jsonl")).unwrap(),
            foreign_raw_before
        );
        assert_eq!(std::fs::read(&colliding_legacy).unwrap(), legacy_before);
        assert_eq!(std::fs::read(&allowed_legacy).unwrap(), allowed_before);
        assert_eq!(std::fs::read(&rejected_legacy).unwrap(), rejected_before);
        assert_eq!(std::fs::read(&traversal_legacy).unwrap(), traversal_before);

        let structured_id = polaris_core::session_store::new_session_id().expect("id");
        let mut state = ConversationStateV2::new("project".into(), structured_id.clone())
            .expect("structured state");
        state.history_mode = HistoryMode::Structured10;
        ConversationStore::create(dir.path(), state).expect("write structured fixture");
        let raw_before = std::fs::read(
            dir.path()
                .join("sessions-v2")
                .join(&structured_id)
                .join("raw.jsonl"),
        )
        .expect("structured raw");
        let state_before = std::fs::read(
            dir.path()
                .join("sessions-v2")
                .join(&structured_id)
                .join("state.json"),
        )
        .expect("structured state");
        let database_before_structured = std::fs::read(&database).expect("database bytes");
        // An explicit structured target resolves without writing anything.
        assert_eq!(
            resolve_continuation(
                Some(&structured_id),
                None,
                dir.path(),
                "project",
                None,
                HistoryMode::Legacy,
                &dir.path().join("sessions"),
            )
            .expect("structured target")
            .expect("continuation")
            .history_mode,
            HistoryMode::Structured10
        );
        assert_eq!(
            std::fs::read(
                dir.path()
                    .join("sessions-v2")
                    .join(&structured_id)
                    .join("raw.jsonl"),
            )
            .unwrap(),
            raw_before
        );
        assert_eq!(
            std::fs::read(
                dir.path()
                    .join("sessions-v2")
                    .join(&structured_id)
                    .join("state.json"),
            )
            .unwrap(),
            state_before
        );
        assert_eq!(
            std::fs::read(&database).unwrap(),
            database_before_structured
        );
    }

    #[test]
    fn saved_history_mode_overrides_config_and_explicit_conflicts_do_not_write() {
        use polaris_core::{conversation_state::HistoryMode, session_store::PersistedSession};
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("memory.sqlite3");
        let legacy =
            PersistedSession::create(dir.path(), &database, "project", HistoryMode::Legacy, None)
                .expect("legacy source");
        let strict = PersistedSession::create(
            dir.path(),
            &database,
            "project",
            HistoryMode::Strict10,
            None,
        )
        .expect("strict source");
        let legacy_id = legacy.snapshot().unwrap().state.session_id;
        let strict_id = strict.snapshot().unwrap().state.session_id;
        let strict_resume = resolve_continuation(
            Some(&strict_id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(strict_resume.history_mode, HistoryMode::Strict10);
        let strict_fork = resolve_continuation(
            None,
            Some(&strict_id),
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(strict_fork.history_mode, HistoryMode::Strict10);
        let legacy_resume = resolve_continuation(
            Some(&legacy_id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Strict10,
            &dir.path().join("sessions"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(legacy_resume.history_mode, HistoryMode::Legacy);
        let legacy_fork = resolve_continuation(
            None,
            Some(&legacy_id),
            dir.path(),
            "project",
            None,
            HistoryMode::Strict10,
            &dir.path().join("sessions"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(legacy_fork.history_mode, HistoryMode::Legacy);

        let database_before = std::fs::read(&database).expect("database bytes");
        let sessions_before = std::fs::read_dir(dir.path().join("sessions-v2"))
            .expect("v2 sessions")
            .count();
        for (resume, fork, requested) in [
            (Some(legacy_id.as_str()), None, HistoryMode::Strict10),
            (None, Some(legacy_id.as_str()), HistoryMode::Strict10),
            (Some(strict_id.as_str()), None, HistoryMode::Legacy),
            (None, Some(strict_id.as_str()), HistoryMode::Legacy),
        ] {
            assert!(
                resolve_continuation(
                    resume,
                    fork,
                    dir.path(),
                    "project",
                    Some(requested),
                    HistoryMode::Legacy,
                    &dir.path().join("sessions"),
                )
                .is_err()
            );
        }
        assert_eq!(std::fs::read(&database).unwrap(), database_before);
        assert_eq!(
            std::fs::read_dir(dir.path().join("sessions-v2"))
                .unwrap()
                .count(),
            sessions_before
        );
        assert_eq!(legacy.snapshot().unwrap().state.session_id, legacy_id);
        assert_eq!(strict.snapshot().unwrap().state.session_id, strict_id);
    }

    use super::*;

    fn routine_project() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("crate-a/src")).unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate-a\"]\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crate-a/src/lib.rs"),
            "pub fn value() {}\n",
        )
        .unwrap();
        let status = std::process::Command::new(resolve_cargo_program().unwrap())
            .current_dir(directory.path())
            .args(["generate-lockfile", "--offline"])
            .status()
            .unwrap();
        assert!(status.success());
        directory
    }

    fn routine_path_dependency_project() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("crate-a/src")).unwrap();
        std::fs::create_dir_all(directory.path().join("crate-b/src")).unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"crate-a\", \"crate-b\"]\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncrate-b = { path = \"../crate-b\" }\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crate-a/src/lib.rs"),
            "pub fn value() -> usize { crate_b::value() }\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crate-b/Cargo.toml"),
            "[package]\nname = \"crate-b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crate-b/src/lib.rs"),
            "pub fn value() -> usize { 1 }\n",
        )
        .unwrap();
        let status = std::process::Command::new(resolve_cargo_program().unwrap())
            .current_dir(directory.path())
            .args(["generate-lockfile", "--offline"])
            .status()
            .unwrap();
        assert!(status.success());
        directory
    }

    fn static_owner_for_path_dependency_project(
        project: &Path,
        session_id: &str,
    ) -> (
        polaris_core::session_store::PersistedSession,
        polaris_core::verification::SessionVerificationToolOwnerV1,
    ) {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig},
        };

        let sandbox = build_sandbox_policy(project, SandboxMode::WorkspaceWrite).unwrap();
        let prompt = "Validate the component manifest.";
        let prepared =
            prepare_routine_authority(&static_authority_json(), prompt, project, &sandbox, project)
                .unwrap();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let saved = PersistedSession::create(
            project,
            &project.join(format!("{session_id}.sqlite3")),
            session_id,
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let contract = ChangeContractV1::draft_typed(
            format!("change-{session_id}"),
            prepared.eligibility.clone(),
            prompt,
            "The component manifest may be invalid.",
            "The component manifest is valid.",
            prepared.verification.clone(),
        )
        .unwrap();
        saved
            .publish_and_approve_routine_contract(workflow.state().revision, contract)
            .unwrap();
        let observer = Arc::new(CliVerificationObserverV1 {
            project_root: project.canonicalize().unwrap(),
            component_root: prepared.component_root.clone(),
            subject_hash: prepared.subject_hash.clone(),
            cargo_program: resolve_cargo_program().unwrap(),
        });
        let owner = polaris_core::verification::SessionVerificationToolOwnerV1::new(
            saved.clone(),
            observer,
            prepared.runners,
        )
        .unwrap()
        .with_preparation(prepared.verification_preparation)
        .unwrap()
        .with_static_adapters(prepared.static_adapters)
        .unwrap();
        (saved, owner)
    }

    fn routine_authority_json(program: &str) -> String {
        serde_json::json!({
            "schema_version": 1,
            "component_root": "crate-a",
            "security": false,
            "migration": false,
            "public_api": false,
            "dependency": false,
            "distribution": false,
            "major_user_contract": false,
            "unknown_impact": false,
            "criteria": [{
                "description": "The focused case passes.",
                "observable_failure": "The focused case fails.",
                "evidence_kind": "common_case",
                "common_runner_plan": {
                    "version": 1,
                    "argv": [program],
                    "stdin": null,
                    "cases": [{"case":"exits_with","status":0}]
                },
                "timeout_secs": 5
            }]
        })
        .to_string()
    }

    fn existing_test_authority_json(program: Option<&str>) -> String {
        use polaris_core::verification::{TestCostClassV1, TestEntryV1, TestInventoryV1};
        let inventory = TestInventoryV1::complete_rust(vec![TestEntryV1 {
            id: "rust-crate-a-lib-focused".into(),
            framework: "rust".into(),
            package: "crate-a".into(),
            target: "lib".into(),
            exact_name: "tests::focused".into(),
            source_location: None,
            dynamic_or_unknown: false,
            cost: TestCostClassV1::Fast,
            symbols: Default::default(),
            common_case_ids: Default::default(),
        }])
        .unwrap();
        let mut value = serde_json::json!({
            "schema_version": 1,
            "component_root": "crate-a",
            "security": false,
            "migration": false,
            "public_api": false,
            "dependency": false,
            "distribution": false,
            "major_user_contract": false,
            "unknown_impact": false,
            "criteria": [{
                "description": "The registered Rust test passes.",
                "observable_failure": "The registered Rust test fails.",
                "evidence_kind": "existing_test"
            }]
        });
        if let Some(program) = program {
            value["existing_test"] = serde_json::json!({
                "cargo_program": program,
                "timeout_secs": 30,
                "inventory": inventory
            });
        }
        value.to_string()
    }

    fn static_authority_json() -> String {
        serde_json::json!({
            "schema_version": 1,
            "component_root": "crate-a",
            "security": false,
            "migration": false,
            "public_api": false,
            "dependency": false,
            "distribution": false,
            "major_user_contract": false,
            "unknown_impact": false,
            "criteria": [{
                "description": STATIC_COMPONENT_MANIFEST_DESCRIPTION,
                "observable_failure": STATIC_COMPONENT_MANIFEST_FAILURE,
                "evidence_kind": "static"
            }]
        })
        .to_string()
    }

    fn review_authority_json(revision: u64, checkpoint_id: &str) -> String {
        serde_json::json!({
            "schema_version": 1,
            "expected_workflow_revision": revision,
            "checkpoint_id": checkpoint_id,
            "reviewer_id": "cli-review-owner",
            "findings": [{
                "finding_id": "finding-1",
                "disposition": "resolved",
                "resolution_ref": "change:focused-fix"
            }]
        })
        .to_string()
    }

    #[test]
    fn m27_review_authority_is_strict_and_requires_closed_dispositions() {
        let valid = review_authority_json(7, "checkpoint-1");
        assert!(prepare_review_authority(&valid).is_ok());

        let mut unknown: serde_json::Value = serde_json::from_str(&valid).unwrap();
        unknown["binding"] = serde_json::json!({"source_hash":"untrusted"});
        assert!(prepare_review_authority(&unknown.to_string()).is_err());

        let mut open: serde_json::Value = serde_json::from_str(&valid).unwrap();
        open["findings"][0]["disposition"] = serde_json::json!("open");
        open["findings"][0]
            .as_object_mut()
            .unwrap()
            .remove("resolution_ref");
        assert!(prepare_review_authority(&open.to_string()).is_err());

        let mut missing_ref: serde_json::Value = serde_json::from_str(&valid).unwrap();
        missing_ref["findings"][0]
            .as_object_mut()
            .unwrap()
            .remove("resolution_ref");
        assert!(prepare_review_authority(&missing_ref.to_string()).is_err());
    }

    #[test]
    fn m27_routine_authority_is_strict_and_builds_one_host_fixed_component() {
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        let prepared = prepare_routine_authority(
            &routine_authority_json("/usr/bin/true"),
            "Fix the focused case.",
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        assert_eq!(prepared.eligibility.affected_components, ["crate-a"]);
        assert_eq!(prepared.verification.len(), 1);
        assert_eq!(prepared.runners.len(), 1);

        let mut value: serde_json::Value =
            serde_json::from_str(&routine_authority_json("/usr/bin/true")).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(
            prepare_routine_authority(
                &value.to_string(),
                "Fix the focused case.",
                project.path(),
                &sandbox,
                project.path(),
            )
            .is_err()
        );
    }

    #[test]
    fn m27_routine_authority_refuses_cross_component_or_non_routine_scope() {
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        for (field, value) in [
            ("component_root", serde_json::json!(".")),
            ("public_api", serde_json::json!(true)),
        ] {
            let mut authority: serde_json::Value =
                serde_json::from_str(&routine_authority_json("/usr/bin/true")).unwrap();
            authority[field] = value;
            assert!(
                prepare_routine_authority(
                    &authority.to_string(),
                    "Fix the focused case.",
                    project.path(),
                    &sandbox,
                    project.path(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn m27_routine_authority_fails_closed_when_fixed_runner_is_absent() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{Phase, SessionWorkflow, WorkflowConfig},
        };
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        let prompt = "Fix the focused case.";
        let prepared = prepare_routine_authority(
            &routine_authority_json("/path/that/does/not/exist"),
            prompt,
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let persistence = PersistedSession::create(
            project.path(),
            &project.path().join("memory.sqlite3"),
            "routine-test",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut session = Session {
            workflow: Some(workflow),
            ..Session::default()
        };
        session.attach(persistence).unwrap();
        assert!(install_routine_authority(&mut session, prompt, prepared, project.path()).is_err());
        assert!(session.verification_owner.is_none());
    }

    #[test]
    fn m27_routine_authority_resume_reuses_matching_durable_contract_and_owner() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig},
        };
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        let prompt = "Fix the focused case.";
        let prepared = prepare_routine_authority(
            &routine_authority_json("/usr/bin/true"),
            prompt,
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let persistence = PersistedSession::create(
            project.path(),
            &project.path().join("memory.sqlite3"),
            "routine-resume-test",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let contract = ChangeContractV1::draft_typed(
            "change-routine-resume",
            prepared.eligibility.clone(),
            prompt,
            "A bounded defect remains.",
            "The bounded defect is fixed.",
            prepared.verification.clone(),
        )
        .unwrap();
        let restored = persistence
            .publish_and_approve_routine_contract(workflow.state().revision, contract)
            .unwrap();
        let mut session = Session {
            workflow: Some(restored),
            ..Session::default()
        };
        session.attach(persistence).unwrap();
        install_routine_authority(&mut session, prompt, prepared, project.path()).unwrap();
        assert!(session.routine_change_authorization.is_none());
        assert!(session.verification_owner.is_some());
    }

    #[test]
    fn m27_existing_test_requires_and_installs_one_host_executor() {
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        let prompt = "Fix the registered Rust behavior.";
        assert!(
            prepare_routine_authority(
                &existing_test_authority_json(None),
                prompt,
                project.path(),
                &sandbox,
                project.path(),
            )
            .is_err()
        );
        let prepared = prepare_routine_authority(
            &existing_test_authority_json(Some("/usr/bin/true")),
            prompt,
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        assert!(prepared.runners.is_empty());
        assert!(prepared.existing_test_executor.is_some());
        assert!(prepared.verification_preparation.inventory.is_some());
        assert_eq!(
            prepared.verification[0].evidence_kinds,
            [polaris_core::workflow::CriterionEvidenceKindV1::ExistingTest]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn m26_static_authority_registers_only_the_fixed_component_manifest_adapter() {
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        let prepared = prepare_routine_authority(
            &static_authority_json(),
            "Validate the component manifest.",
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        assert!(prepared.runners.is_empty());
        assert!(prepared.existing_test_executor.is_none());
        assert_eq!(prepared.static_adapters.len(), 1);
        assert_eq!(
            prepared.verification[0].evidence_kinds,
            [polaris_core::workflow::CriterionEvidenceKindV1::Static]
                .into_iter()
                .collect()
        );

        let mut arbitrary: serde_json::Value =
            serde_json::from_str(&static_authority_json()).unwrap();
        arbitrary["criteria"][0]["description"] = serde_json::json!("Any arbitrary claim.");
        assert!(
            prepare_routine_authority(
                &arbitrary.to_string(),
                "Validate anything.",
                project.path(),
                &sandbox,
                project.path(),
            )
            .is_err()
        );
        let mut partial: serde_json::Value =
            serde_json::from_str(&static_authority_json()).unwrap();
        partial["criteria"][0]["timeout_secs"] = serde_json::json!(5);
        assert!(
            prepare_routine_authority(
                &partial.to_string(),
                "Validate the manifest.",
                project.path(),
                &sandbox,
                project.path(),
            )
            .is_err()
        );
    }

    #[test]
    fn cargo_lock_change_after_prepare_rejects_the_static_run() {
        use polaris_core::verification::VerificationToolOwnerV1;

        let project = routine_path_dependency_project();
        let (saved, owner) =
            static_owner_for_path_dependency_project(project.path(), "static-lock-binding");
        let prepared = owner.prepare().unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        saved
            .publish_verification_checkpoint(generation, prepared.clone())
            .unwrap();
        let lock = project.path().join("Cargo.lock");
        let mut contents = std::fs::read_to_string(&lock).unwrap();
        contents.push_str("\n# binding changed\n");
        std::fs::write(lock, contents).unwrap();
        assert!(owner.begin_run(&prepared.checkpoint_id).is_err());
    }

    #[test]
    fn sibling_path_dependency_change_after_run_is_unknown_and_cannot_publish() {
        use polaris_core::verification::{VerificationCheckpointStageV1, VerificationToolOwnerV1};

        let project = routine_path_dependency_project();
        let (saved, owner) =
            static_owner_for_path_dependency_project(project.path(), "static-path-binding");
        let prepared = owner.prepare().unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        saved
            .publish_verification_checkpoint(generation, prepared.clone())
            .unwrap();
        owner.begin_run(&prepared.checkpoint_id).unwrap();
        std::fs::write(
            project.path().join("crate-b/src/lib.rs"),
            "pub fn value() -> usize { 2 }\n",
        )
        .unwrap();
        let observed = owner.poll_run(&prepared.checkpoint_id).unwrap();
        assert_eq!(
            observed.stage,
            VerificationCheckpointStageV1::OutcomeUnknown
        );
        assert!(owner.publish(&prepared.checkpoint_id).is_err());
    }

    #[test]
    fn m27_existing_test_executor_identity_change_is_rejected_after_prepare() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig},
        };
        let project = routine_project();
        let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
        let prompt = "Fix the registered Rust behavior.";
        let first = prepare_routine_authority(
            &existing_test_authority_json(Some("/usr/bin/true")),
            prompt,
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let persistence = PersistedSession::create(
            project.path(),
            &project.path().join("memory.sqlite3"),
            "existing-test-identity",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let contract = ChangeContractV1::draft_typed(
            "change-existing-test-identity",
            first.eligibility.clone(),
            prompt,
            "A registered behavior regressed.",
            "The registered behavior is restored.",
            first.verification.clone(),
        )
        .unwrap();
        let restored = persistence
            .publish_and_approve_routine_contract(workflow.state().revision, contract)
            .unwrap();
        let mut first_session = Session {
            workflow: Some(restored.clone()),
            ..Session::default()
        };
        first_session.attach(persistence.clone()).unwrap();
        install_routine_authority(&mut first_session, prompt, first, project.path()).unwrap();
        let checkpoint = first_session
            .verification_owner
            .as_ref()
            .unwrap()
            .prepare()
            .unwrap();

        let second = prepare_routine_authority(
            &existing_test_authority_json(Some("/bin/echo")),
            prompt,
            project.path(),
            &sandbox,
            project.path(),
        )
        .unwrap();
        let mut resumed = Session {
            workflow: Some(restored),
            ..Session::default()
        };
        resumed.attach(persistence).unwrap();
        install_routine_authority(&mut resumed, prompt, second, project.path()).unwrap();
        assert!(
            resumed
                .verification_owner
                .as_ref()
                .unwrap()
                .begin_run(&checkpoint.checkpoint_id)
                .is_err()
        );
    }

    #[test]
    fn default_provider_prefers_openai_when_its_key_is_available() {
        assert_eq!(default_provider_name(true, true), "openai");
        assert_eq!(default_provider_name(true, false), "openai");
    }

    #[test]
    fn default_provider_falls_back_to_saved_codex_credentials() {
        // The bug this pins down: signing in with ChatGPT through
        // onboarding saves `~/.polaris/auth.json` but nothing sets
        // `POLARIS_PROVIDER`. Without this fallback, the next launch
        // defaults straight back to "openai", finds no key, and reopens
        // onboarding even though the user already signed in.
        assert_eq!(default_provider_name(false, true), "codex");
    }

    #[test]
    fn defaults_and_explicit_model_effort_have_the_documented_precedence() {
        let args = Args::try_parse_from(["polaris", "-p", "hello"]).expect("args");
        assert_eq!(selected_model(args.model, None), "gpt-6-astra");
        // The orchestrator — the root run itself — is astra at xhigh.
        // Delegations are priced by `[tier.*]`, which is where the cheaper
        // models live; the root run is the one party whose own reasoning
        // nothing downstream can make up for.
        assert_eq!(args.effort, None);
        assert_eq!(selected_effort(None, true), Some("xhigh"));
        assert_eq!(selected_effort(None, false), None);
        // The TUI marks a row "(default)" by display name, so the row it
        // marks has to be the one this default actually selects.
        assert_eq!(
            polaris_tui::render::effort_wire_value(polaris_tui::render::DEFAULT_EFFORT),
            "xhigh"
        );
        assert_eq!(selected_model(None, Some("env-model".into())), "env-model");
        assert_eq!(
            selected_model(Some("flag-model".into()), Some("env-model".into())),
            "flag-model"
        );
        let args = Args::try_parse_from(["polaris", "--effort", "high"]).expect("args");
        assert_eq!(args.effort.as_deref(), Some("high"));
        // `max` is real on `/responses`; `ultra` is not an effort the API
        // has, and offering it only meant sending something else instead.
        assert_eq!(
            Args::try_parse_from(["polaris", "--effort", "max"])
                .expect("args")
                .effort
                .as_deref(),
            Some("max")
        );
        assert!(Args::try_parse_from(["polaris", "--effort", "ultra"]).is_err());
    }

    #[test]
    fn workflow_history_and_durable_session_flags_parse_with_their_contract() {
        let args = Args::try_parse_from([
            "polaris",
            "--phase",
            "implement",
            "--history-mode",
            "strict10",
            "-p",
            "continue",
        ])
        .expect("v0.11 flags");
        assert_eq!(args.phase, Some(PhaseArg::Implement));
        assert_eq!(args.history_mode, Some(HistoryModeArg::Strict10));
        assert!(args.resume.is_none());
        assert!(
            Args::try_parse_from([
                "polaris", "--resume", "one", "--fork", "two", "-p", "continue",
            ])
            .is_err()
        );
        for flag in ["--resume", "--fork"] {
            assert!(
                Args::try_parse_from([
                    "polaris",
                    flag,
                    "00000000-0000-4000-8000-000000000001",
                    "--phase",
                    "review",
                ])
                .is_err()
            );
        }
        let structured = Args::try_parse_from([
            "polaris",
            "--history-mode",
            "structured10",
            "-p",
            "continue",
        ])
        .unwrap();
        assert_eq!(structured.history_mode, Some(HistoryModeArg::Structured10));
    }

    #[test]
    fn default_provider_falls_back_to_openai_when_neither_credential_exists() {
        // openai is the arm that owns onboarding; codex's own arm just
        // fails lazily on first use. With nothing saved anywhere, we want
        // onboarding to fire, so this must stay "openai".
        assert_eq!(default_provider_name(false, false), "openai");
    }

    #[test]
    fn confined_read_parses_without_prompt_and_conflicts_with_mutation() {
        let args = Args::try_parse_from(["polaris", "--confined-read"]).unwrap();
        assert!(args.confined_read);
        assert!(Args::try_parse_from(["polaris", "--confined-read", "--confined-apply"]).is_err());
    }

    #[test]
    fn confined_apply_parses_without_a_prompt() {
        // --confined-apply is the path that reads a mutation operation from
        // stdin, and doesn't need an instruction. If this stayed required,
        // nobody could reach the helper entry point Task 7 built.
        let args = Args::try_parse_from(["polaris", "--confined-apply"])
            .expect("could not parse with just --confined-apply");
        assert!(args.confined_apply);
        assert!(args.prompt.is_none());
    }

    #[test]
    fn prompt_parses_as_optional_at_the_clap_level() {
        // We don't use `required_unless_present`. It only looks at argument
        // names, not whether a subcommand is present, so adding a subcommand
        // here would get `polaris login` rejected for "missing --prompt"
        // (the same trap as M2 Task 8). At the clap level, `prompt` is
        // always optional; that it's "required on the normal path" is
        // verified by launching the real binary in
        // `tests/subcommands.rs::the_normal_path_still_requires_a_prompt`.
        // Here we only pin down clap's parse result.
        let args = Args::try_parse_from(["polaris"]).expect("clap should not make prompt required");
        assert!(args.prompt.is_none());
    }

    #[test]
    fn tui_can_defer_routine_and_review_authority_until_its_first_input() {
        let args = Args::try_parse_from([
            "polaris",
            "--routine-authority",
            "{}",
            "--review-authority",
            "{}",
        ])
        .expect("interactive authority must not require --prompt");
        assert!(args.prompt.is_none());
        assert_eq!(args.routine_authority.as_deref(), Some("{}"));
        assert_eq!(args.review_authority.as_deref(), Some("{}"));
        assert!(
            Args::try_parse_from(["polaris", "--review-authority", "{}"]).is_err(),
            "review authority still requires routine authority"
        );
    }

    #[test]
    fn skipped_skills_are_reported_one_line_each_naming_directory_and_cause() {
        // Directly verifies the formatting logic that hands the skipped
        // entries discover collected to stderr one line each, without
        // swallowing them. Verifying stderr by launching the real process
        // would mean going all the way down the path that requires an API
        // key, so here we pull out and verify just the formatting function.
        //
        // Looking only at the directory name, formatting that discards the
        // cause entirely (printing just `{s.dir_name}`) would also pass.
        // This function is the only thing standing between "a skill silently
        // vanished" and the user, so we also check that the cause is
        // present, and that "couldn't be read" isn't confused with "failed
        // validation". The two entries use different SkipCause variants.
        let skipped = vec![
            polaris_skills::Skipped {
                dir_name: "unreadable-one".into(),
                cause: polaris_skills::SkipCause::Unreadable(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "no permission",
                )),
            },
            polaris_skills::Skipped {
                dir_name: "invalid-two".into(),
                cause: polaris_skills::SkipCause::Invalid(
                    polaris_skills::SkillError::InvalidName {
                        name: "Invalid-Two".into(),
                    },
                ),
            },
        ];

        let lines = format_skipped_skills(&skipped);

        assert_eq!(lines.len(), 2, "not one line each: {lines:?}");
        assert!(
            lines[0].contains("unreadable-one"),
            "directory name is missing: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("no permission"),
            "the reason it couldn't be read is missing: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("invalid-two"),
            "directory name is missing: {}",
            lines[1]
        );
        assert!(
            lines[1].contains("naming rules"),
            "the reason validation failed is missing: {}",
            lines[1]
        );
        // Each line carries only its own cause. Formatting that writes every
        // cause into every line would ultimately fail to convey which skill
        // vanished and why.
        assert!(
            !lines[0].contains("naming rules") && !lines[1].contains("no permission"),
            "causes are mixed across lines: {lines:?}"
        );
    }

    #[test]
    fn nothing_is_printed_when_no_skill_was_skipped() {
        // If even one line came out when nothing was skipped, the user would
        // end up chasing a failure that doesn't exist. This pins down empty
        // input producing empty output directly, rather than as a side
        // effect of `.map().collect()`.
        assert!(
            format_skipped_skills(&[]).is_empty(),
            "output exists even though no skill was skipped"
        );
    }

    /// If tests that swap out HOME run concurrently in the same process,
    /// they step on each other's HOME. Serialize the swap here.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Swaps HOME, runs `f`, and always restores it afterward.
    ///
    /// SAFETY: This only swaps HOME temporarily within this process; it
    /// doesn't affect other processes. `std::env::set_var`'s safety
    /// condition is concurrent reads/writes from multiple threads, and
    /// that's serialized by `HOME_LOCK` above.
    fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home);
        }
        let out = f();
        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn managed_runtime_home_is_independent_from_custom_data_dir() {
        let state = tempfile::tempdir().expect("state");
        let fake_home = tempfile::tempdir().expect("fake home");
        with_home(fake_home.path(), || {
            let previous = std::env::var_os("POLARIS_DATA_DIR");
            unsafe { std::env::set_var("POLARIS_DATA_DIR", state.path()) };
            let resolved = managed_runtime_home().expect("runtime home");
            match previous {
                Some(value) => unsafe { std::env::set_var("POLARIS_DATA_DIR", value) },
                None => unsafe { std::env::remove_var("POLARIS_DATA_DIR") },
            }
            let actual = polaris_core::os_home::trusted_user_home().expect("actual OS home");
            assert_eq!(resolved, actual);
            assert_eq!(
                resolved.join(".polaris/runtime-images/python-e5-cat"),
                actual.join(".polaris/runtime-images/python-e5-cat")
            );
            assert!(!resolved.starts_with(state.path()));
            assert!(!resolved.starts_with(fake_home.path()));
        });
    }

    #[test]
    fn one_project_has_one_state_dir_no_matter_how_deep_you_start() {
        // The writable root is derived from `project::resolve_root`, but the
        // state directory alone used to hash the working directory
        // directly. Launching the same project from deep inside it would
        // produce separate locations for the audit log and the helper's
        // staging area. The audit log is the record of reconstruction, so
        // if it splits, the history never comes together anywhere.
        let home = tempfile::tempdir().expect("temp directory");
        let project = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(project.path().join(".git")).expect("mkdir");
        let deep = project.path().join("crates/polaris-core/src");
        std::fs::create_dir_all(&deep).expect("mkdir");
        let other = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(other.path().join(".git")).expect("mkdir");

        let (from_root, from_deep, from_other) = with_home(home.path(), || {
            (
                polaris_core::project::state_dir(project.path())
                    .expect("could not determine the default path"),
                polaris_core::project::state_dir(&deep)
                    .expect("could not determine the default path"),
                polaris_core::project::state_dir(other.path())
                    .expect("could not determine the default path"),
            )
        });

        assert_eq!(
            from_root,
            from_deep,
            "the state directory splits based on launch depth: {} and {}",
            from_root.display(),
            from_deep.display()
        );
        // Reject a degenerate "fix" that just funnels everything into the
        // same place. Different projects must get different directories.
        assert_ne!(
            from_root,
            from_other,
            "different projects are sharing the same state directory: {}",
            from_root.display()
        );
    }

    /// The arm exists and names itself in the error a person will see.
    /// Until 2026-09-19 the CLI had no local provider at all — local
    /// models were reachable only from the desktop's role bindings — so
    /// the measurement harnesses, which drive the CLI, could not run one.
    #[test]
    fn the_unknown_provider_message_lists_every_arm_that_exists() {
        // Read out of the source rather than written here twice: a new
        // arm that the message does not mention is a provider nobody can
        // find, and that is exactly how `llamacpp` would have shipped.
        let source = include_str!("main.rs");
        let arms: std::collections::BTreeSet<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_suffix(" => {"))
            .filter_map(|name| name.strip_prefix('"'))
            .filter_map(|name| name.strip_suffix('"'))
            .filter(|name| ["openai", "codex", "llamacpp"].contains(name))
            .collect();
        assert_eq!(
            arms,
            ["codex", "llamacpp", "openai"].into_iter().collect(),
            "an arm was added or removed without this test noticing"
        );
        let message = source
            .lines()
            .find(|line| line.contains("POLARIS_PROVIDER is an unknown value"))
            .expect("the unknown-provider message is gone");
        for arm in &arms {
            assert!(
                source
                    .lines()
                    .skip_while(|line| !line.contains("POLARIS_PROVIDER is an unknown value"))
                    .take(3)
                    .any(|line| line.contains(arm)),
                "{arm} is not named in the unknown-provider message: {message}"
            );
        }
    }

    #[test]
    fn default_audit_path_lives_under_home_state_not_cwd() {
        let home = tempfile::tempdir().expect("temp directory");
        let project = tempfile::tempdir().expect("temp directory");

        let got = with_home(home.path(), || {
            default_audit_path(project.path()).expect("could not determine the default path")
        });

        assert!(
            got.starts_with(home.path()),
            "audit log is not under the home directory: {}",
            got.display()
        );
        assert!(
            !got.starts_with(project.path()),
            "audit log is inside the working directory: {}",
            got.display()
        );
        assert_eq!(got.file_name().unwrap(), "audit.jsonl");
        assert!(got.parent().unwrap().is_dir(), "directory was not created");
    }
}

#[cfg(test)]
mod tool_memory_settings_tests {
    use super::*;

    /// The configured judge is the one that answers, and a judge that
    /// cannot start yields none rather than the other one.
    ///
    /// The env var is set inside the test because the Jev branch reads
    /// it: without it the second case would pass for the wrong reason,
    /// reporting "no key" where the property under test is "no silent
    /// substitution". Nothing else in this binary reads that name.
    #[test]
    fn a_configured_backend_answers_or_nothing_does() {
        use polaris_core::config::{DecisionBackend, DecisionConfig};

        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("semif-helper");
        std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
        // SAFETY: single-threaded test, and no other test in this binary
        // reads or writes this variable.
        unsafe { std::env::set_var("POLARIS_JEV_API_KEY", "test-key") };

        let semif = DecisionConfig {
            enabled: true,
            backend: DecisionBackend::SemIf,
            helper: Some(helper.clone()),
            ..Default::default()
        };
        assert_eq!(
            build_decider(&semif).map(|d| d.name().to_owned()),
            Some("semif".to_owned())
        );

        let missing = DecisionConfig {
            helper: Some(dir.path().join("absent")),
            ..semif.clone()
        };
        assert!(
            build_decider(&missing).is_none(),
            "a SemIf that cannot start was answered by the remote judge"
        );

        let jev = DecisionConfig {
            enabled: true,
            backend: DecisionBackend::Jev,
            ..Default::default()
        };
        assert_eq!(
            build_decider(&jev).map(|d| d.name().to_owned()),
            Some("jev".to_owned())
        );

        let off = DecisionConfig {
            enabled: false,
            ..semif
        };
        assert!(build_decider(&off).is_none(), "disabled still built one");
        unsafe { std::env::remove_var("POLARIS_JEV_API_KEY") };
    }

    #[test]
    fn tool_memory_defaults_off_and_is_independent_of_remember() {
        let args = Args::try_parse_from(["polaris"]).unwrap();
        assert_eq!(args.tool_memory.retention(), None);
        assert!(args.validate_tool_memory().is_ok());
        for mode in ["history", "retrieval"] {
            let args = Args::try_parse_from(["polaris", "--tool-memory", mode]).unwrap();
            assert!(args.tool_memory.retention().is_some());
            assert!(!args.remember);
            assert!(args.validate_tool_memory().is_ok());
        }
        let args = Args::try_parse_from(["polaris", "--remember"]).unwrap();
        assert_eq!(args.tool_memory.retention(), None);
        assert!(Args::try_parse_from(["polaris", "--tool-memory", "invalid"]).is_err());
    }

    #[test]
    fn tool_memory_embedding_requires_pair_and_enabled_mode() {
        for flag in [
            "--tool-memory-embedding-url",
            "--tool-memory-embedding-model",
        ] {
            assert!(Args::try_parse_from(["polaris", flag, "value"]).is_err());
        }
        for mode in ["off", "history", "retrieval"] {
            let args = Args::try_parse_from([
                "polaris",
                "--tool-memory",
                mode,
                "--tool-memory-embedding-url",
                "http://localhost:8080/v1",
                "--tool-memory-embedding-model",
                "local-model",
            ])
            .unwrap();
            assert_eq!(args.validate_tool_memory().is_ok(), mode != "off");
        }
        let args = Args::try_parse_from([
            "polaris",
            "--tool-memory",
            "retrieval",
            "--tool-memory-embedding-url",
            " ",
            "--tool-memory-embedding-model",
            "model",
        ])
        .unwrap();
        assert!(args.validate_tool_memory().is_err());
    }

    #[test]
    fn tool_memory_help_lists_settings() {
        let help = <Args as clap::CommandFactory>::command()
            .render_long_help()
            .to_string();
        for flag in [
            "--tool-memory",
            "--tool-memory-embedding-url",
            "--tool-memory-embedding-model",
        ] {
            assert!(help.contains(flag));
        }
        assert!(help.contains("off, history, retrieval"));
    }

    #[tokio::test]
    async fn tui_answer_finalizer_obeys_off_shadow_and_enforce_modes() {
        use polaris_core::config::{
            AnswerFinalizerMode, JudgeProfile, PresentationLanguage, TranslationActivation,
            TranslationProfile,
        };

        let root = tempfile::tempdir().unwrap();
        let mut config = polaris_core::config::Config::default();
        assert!(
            build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
                .await
                .unwrap()
                .is_none()
        );

        config.presentation.language = PresentationLanguage::Ja;
        config.answer_finalizer.mode = AnswerFinalizerMode::Shadow;
        config.answer_finalizer.judge_profile = Some(JudgeProfile::FulfillmentV1);
        assert!(
            build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
                .await
                .unwrap()
                .is_some()
        );

        config.answer_finalizer.translation.activation = TranslationActivation::Requested;
        config.answer_finalizer.translation.profile = Some(TranslationProfile::CatTranslate3_3b);
        assert!(
            build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
                .await
                .unwrap()
                .is_some(),
            "a missing managed model remains an observational shadow failure"
        );

        config.answer_finalizer.mode = AnswerFinalizerMode::Enforce;
        assert!(
            build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
                .await
                .is_err()
        );
    }
}
