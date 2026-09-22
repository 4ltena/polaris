//! Entry point for the `polaris` binary. Decides the endpoint from environment
//! variables, assembles the always-on context, and runs the agent loop once.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
  POLARIS_MODEL     Defaults to gpt-6-astra; reasoning effort defaults to xhigh (--effort)
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

    /// 推論強度。既定はxhigh。明示した値が優先する。
    ///
    /// この既定はorchestrator——rootの実行自身——の強度である。委譲先の
    /// 強度は`[tier.*]`が決め、そこで強度を書かなかった段だけがこの値を
    /// 引き継ぐ。`max`はResponses API限定だが、polarisの主経路は
    /// `/responses`なので送れる。`ultra`はAPIに無いので受け付けない。
    #[arg(long, default_value = "xhigh", value_parser = ["low", "medium", "high", "xhigh", "max"])]
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

fn strict_history(
    config: &polaris_core::config::EmbeddingConfig,
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
    let mut embedder = polaris_core::strict_provider::local_embedder(config, private_root)
        .map_err(|e| e.to_string())?;
    embedder.attempt_ledger = Some(attempt_ledger);
    Ok((
        build_strict_history(summary, std::sync::Arc::new(embedder), decider),
        usage,
    ))
}

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

fn build_decider(
    config: &polaris_core::config::DecisionConfig,
) -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
    if !config.enabled {
        return None;
    }
    let key = match std::env::var("POLARIS_JEV_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            eprintln!(
                "[decision] enabled = true ですが POLARIS_JEV_API_KEY がありません。判定器なしで実行します"
            );
            return None;
        }
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
        Ok(decider) => Some(std::sync::Arc::new(decider)),
        Err(error) => {
            eprintln!("判定器を作れません（{error:?}）。判定器なしで実行します");
            None
        }
    }
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
    // Mirror the CLI effort in the TUI footer.
    let mut initial_effort_name: Option<String> = None;
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
                let weights =
                    tier_providers(&config.tiers, &model, args.effort.as_deref(), |named| {
                        OpenAiProvider::new(base.clone(), key.clone(), named.to_owned())
                            .map(|provider| {
                                std::sync::Arc::new(
                                    provider.with_attempt_ledger(attempt_ledger.clone()),
                                )
                                    as std::sync::Arc<dyn polaris_provider::Provider>
                            })
                            .map_err(|error| error.to_string())
                    });
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
                    std::sync::Arc::new(provider.with_attempt_ledger(attempt_ledger.clone()))
                        as std::sync::Arc<dyn polaris_provider::Provider>
                });
                let weights =
                    tier_providers(&config.tiers, &model, args.effort.as_deref(), |named| {
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
                    });
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

    if let Some(effort) = args.effort.as_deref() {
        provider.set_effort(Some(effort));
        initial_effort_name = Some(effort.to_string());
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
        let provider_name = provider_name.clone();
        let private_root = data_root.to_path_buf();
        let ledger = attempt_ledger.clone();
        let parent_id = attempt_parent_id.clone();
        let decider = decider.clone();
        std::sync::Arc::new(move || {
            if embedding.runtime.is_none()
                || embedding.model_path.is_none()
                || embedding.revision.is_none()
            {
                return Err(
                    "strict10 には [embedding] の runtime、model_path、revision が必要です".into(),
                );
            }
            strict_history(
                &embedding,
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
                session.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
                    config.workflow.clone(),
                ));
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
        assert_eq!(args.effort.as_deref(), Some("xhigh"));
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
}
