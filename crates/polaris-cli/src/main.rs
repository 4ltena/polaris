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

mod memory;

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
  POLARIS_MODEL     Defaults to gpt-6-astra; reasoning effort defaults to medium (--effort)
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

    /// 推論強度。既定はmedium。明示した値が優先する。
    #[arg(long, default_value = "medium", value_parser = ["low", "medium", "high", "xhigh", "max", "ultra"])]
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
    #[arg(long, value_enum)]
    phase: Option<PhaseArg>,

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

    /// The sandbox policy.
    #[arg(long, value_enum, default_value_t = SandboxModeArg::WorkspaceWrite)]
    sandbox: SandboxModeArg,

    /// The approval boundary policy.
    #[arg(long, value_enum, default_value_t = ApprovalPolicyArg::OnRequest)]
    approval: ApprovalPolicyArg,

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
enum HistoryModeArg {
    Legacy,
    Strict10,
}

impl From<HistoryModeArg> for polaris_core::conversation_state::HistoryMode {
    fn from(value: HistoryModeArg) -> Self {
        match value {
            HistoryModeArg::Legacy => Self::Legacy,
            HistoryModeArg::Strict10 => Self::Strict10,
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
    /// ローカルに保存した会話の検索・取得・削除。
    Memory(memory::Args),
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

fn restore_continuation_workflow(
    resume: Option<&str>,
    fork: Option<&str>,
    session: &mut Session,
    saved: &polaris_core::session_store::PersistedSession,
    config: &polaris_core::workflow::WorkflowConfig,
) -> Result<(), String> {
    if resume.is_none() && fork.is_none() {
        return Ok(());
    }
    if config.enabled {
        session.workflow = Some(
            saved
                .restore_workflow(config.clone())
                .map_err(|error| format!("ワークフロー状態を復元できません: {error}"))?,
        );
    } else {
        let state = saved
            .snapshot()
            .map_err(|error| format!("保存済みワークフローを確認できません: {error}"))?
            .state;
        if state.workflow.revision != 0
            || state.workflow.phase != polaris_core::workflow::Phase::General
            || !state.workflow.skills.is_empty()
        {
            return Err(
                "保存済みワークフローを維持するには設定で [workflow].enabled = true が必要です"
                    .into(),
            );
        }
    }
    Ok(())
}

fn strict_history(
    config: &polaris_core::config::EmbeddingConfig,
    provider_name: &str,
    private_root: &Path,
    attempt_ledger: polaris_provider::attempts::AttemptLedger,
    parent_id: &str,
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
            std::sync::Arc::new(
                polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.into(),
                    "gpt-6-astra".into(),
                    std::sync::Arc::new(AuthTokens {
                        issuer: polaris_auth::ISSUER.into(),
                        store,
                    }),
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
        std::sync::Arc::new(polaris_core::conversation_memory::StrictHistory::new(
            summary,
            std::sync::Arc::new(embedder),
        )),
        usage,
    ))
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
        Some(Command::Memory(options)) => return memory::run(options).await,
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

    if args.confined_apply {
        return run_confined_apply();
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

    let provider: std::sync::Arc<dyn polaris_provider::Provider> = loop {
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
                let model = model.clone();
                model_name = model.clone();
                match OpenAiProvider::new(base, key, model)
                    .map(|provider| provider.with_attempt_ledger(attempt_ledger.clone()))
                {
                    Ok(p) => break std::sync::Arc::new(p),
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
                match polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.to_string(),
                    model,
                    std::sync::Arc::new(AuthTokens {
                        issuer: polaris_auth::ISSUER.to_string(),
                        store,
                    }),
                )
                .map(|provider| provider.with_attempt_ledger(attempt_ledger.clone()))
                {
                    Ok(provider) => break std::sync::Arc::new(provider),
                    Err(error) => {
                        eprintln!("Can't build the client: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                eprintln!("POLARIS_PROVIDER is an unknown value {other}. Specify openai or codex");
                return ExitCode::FAILURE;
            }
        }
    };

    if let Some(effort) = args.effort.as_deref() {
        provider.set_effort(Some(effort));
        initial_effort_name = Some(effort.to_string());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // The audit log and the confined helper's staging location share the
    // same state directory (see the docs on `polaris_core::project::state_dir`).
    let state_dir = match polaris_core::project::state_dir(&cwd) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Can't determine the state directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Every saved conversation lives here, across every project — see the
    // docs on `polaris_core::project::sessions_dir`. Only the interactive
    // TUI path below actually uses it (one-shot `-p`/`exec` runs don't
    // persist a resumable conversation), but it's resolved here alongside
    // `state_dir` since both fail the same way (missing `HOME`).
    let sessions_dir = match polaris_core::project::sessions_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Can't determine the sessions directory: {e}");
            return ExitCode::FAILURE;
        }
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

    let constitution = constitution::load(&cwd);
    let environment = constitution::environment_block(&cwd, None);

    let config = match polaris_core::config::load(&cwd) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("設定を読み込めません: {error}");
            return ExitCode::FAILURE;
        }
    };
    let history_mode = args
        .history_mode
        .map(polaris_core::conversation_state::HistoryMode::from)
        .unwrap_or(config.history_mode);
    if history_mode == polaris_core::conversation_state::HistoryMode::Strict10
        && (config.embedding.runtime.is_none()
            || config.embedding.model_path.is_none()
            || config.embedding.revision.is_none())
    {
        eprintln!("strict10 には [embedding] の runtime、model_path、revision が必要です");
        return ExitCode::FAILURE;
    }
    let configured_strict =
        if history_mode == polaris_core::conversation_state::HistoryMode::Strict10 {
            let Some(root) = sessions_dir.parent() else {
                eprintln!("v2セッションの保存先を特定できません");
                return ExitCode::FAILURE;
            };
            match strict_history(
                &config.embedding,
                &provider_name,
                root,
                attempt_ledger.clone(),
                &attempt_parent_id,
            ) {
                Ok(value) => Some(value),
                Err(error) => {
                    eprintln!("strict10を準備できません: {error}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            None
        };
    let discovered = polaris_skills::discover(&cwd, &config.skills_paths);
    for line in format_skipped_skills(&discovered.skipped) {
        eprintln!("{line}");
    }

    let discovered_agents = polaris_skills::discover_agent_types(&cwd, &config.agents_paths);
    for s in &discovered_agents.skipped {
        eprintln!("agent type skipped: {s}");
    }

    // What rides along on every turn is assembled here exactly once. The
    // assembly itself lives in polaris-core, and the budget tests call the
    // same function. Reassembling or appending to it here would make what
    // production sends diverge from what the tests measure.
    let always_on = prompt::assemble_always_on(&constitution, &environment, &discovered.skills);

    match args.prompt.clone() {
        Some(prompt) => {
            let usage_meter = polaris_provider::UsageMeter::default();
            let provider: std::sync::Arc<dyn polaris_provider::Provider> =
                std::sync::Arc::new(usage_meter.wrap(provider.clone()));
            let mut session = Session::new();
            session.disable_files_md_auto_regenerate = !config.files_md_auto_regenerate;
            if config.workflow.enabled {
                session.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
                    config.workflow.clone(),
                ));
            }
            if let Some(phase) = args.phase.map(Phase::from) {
                let Some(workflow) = &session.workflow else {
                    eprintln!("--phase には設定で [workflow].enabled = true が必要です");
                    return ExitCode::FAILURE;
                };
                if let Err(error) = workflow.request_phase(phase) {
                    eprintln!("ワークフロー段階を設定できません: {error}");
                    return ExitCode::FAILURE;
                }
            }
            let project_id = match polaris_tui::persist::project_identity(
                &polaris_core::project::resolve_root(&cwd),
            ) {
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
            let strict = configured_strict.clone();
            let persistence = match (&args.resume, &args.fork) {
                (Some(id), None) => Some(
                    match polaris_core::session_store::PersistedSession::open(
                        data_root,
                        &database,
                        &project_id,
                        id,
                    ) {
                        Ok(session) => Ok(session),
                        Err(open_error) => {
                            let legacy = sessions_dir.join(format!("{id}.jsonl"));
                            match legacy
                                .is_file()
                                .then(|| polaris_tui::persist::load_session(&legacy))
                            {
                                Some(Ok((legacy_session, _))) => {
                                    polaris_core::session_store::PersistedSession::import(
                                        data_root,
                                        &database,
                                        &project_id,
                                        history_mode,
                                        session.workflow.as_ref(),
                                        &legacy_session.messages,
                                    )
                                }
                                _ => Err(open_error),
                            }
                        }
                    },
                ),
                (None, Some(id)) => Some(
                    polaris_core::session_store::PersistedSession::open(
                        data_root,
                        &database,
                        &project_id,
                        id,
                    )
                    .and_then(|session| session.fork("")),
                ),
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
                )
            {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
            if let Some(persistence) = persistence
                && let Err(error) = session.attach(persistence)
            {
                eprintln!("v2セッションを接続できません: {error}");
                return ExitCode::FAILURE;
            }
            if let Some((history, usage)) = strict {
                session.strict_history = Some(history);
                session.summary_usage = Some(usage);
                if args.resume.is_some()
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
                match polaris_tui::configure_memory(&mut session, &cwd, &state_dir, &session_id) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("履歴保存を準備できません: {error}");
                        return ExitCode::FAILURE;
                    }
                }
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

            match agent::run(
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
            )
            .await
            {
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
                always_on: &always_on,
                skills: &discovered.skills,
                agent_types: &discovered_agents.agent_types,
                spawn_concurrency: config.spawn_concurrency,
                spawn_write_concurrency: config.spawn_write_concurrency,
                files_md_auto_regenerate: config.files_md_auto_regenerate,
                workflow_config: config.workflow,
                initial_phase: args.phase.map(Phase::from),
                history_mode,
                strict_history: configured_strict
                    .as_ref()
                    .map(|(history, _)| history.clone()),
                summary_usage: configured_strict.as_ref().map(|(_, usage)| usage.clone()),
            })
            .await
        }
    }
}

/// Entry point when running as a confined child. Executes the one JSON
/// mutation read from stdin, then exits.
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
    let writable_roots: Vec<PathBuf> = if sandbox_mode == SandboxMode::WorkspaceWrite {
        vec![root]
    } else {
        Vec::new()
    };
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
mod tests {
    #[test]
    fn resume_and_fork_restore_saved_focus_without_resetting_runtime_settings() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{Phase, SessionWorkflow, WorkflowConfig},
        };
        let dir = tempfile::tempdir().unwrap();
        let source_config = WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Review,
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
            )
            .unwrap();
            assert_eq!(
                session.workflow.as_ref().unwrap().state().phase,
                Phase::Review,
                "{flag}"
            );
            assert_eq!(session.workflow.as_ref().unwrap().config, current_config);
            assert!(session.disable_files_md_auto_regenerate);
            let disabled = WorkflowConfig::default();
            assert!(
                restore_continuation_workflow(
                    args.resume.as_deref(),
                    args.fork.as_deref(),
                    &mut session,
                    saved,
                    &disabled
                )
                .is_err(),
                "{flag}"
            );
        }
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
        assert_eq!(args.effort.as_deref(), Some("medium"));
        assert_eq!(selected_model(None, Some("env-model".into())), "env-model");
        assert_eq!(
            selected_model(Some("flag-model".into()), Some("env-model".into())),
            "flag-model"
        );
        let args = Args::try_parse_from(["polaris", "--effort", "high"]).expect("args");
        assert_eq!(args.effort.as_deref(), Some("high"));
    }

    #[test]
    fn workflow_history_and_durable_session_flags_parse_with_their_contract() {
        let args = Args::try_parse_from([
            "polaris",
            "--phase",
            "implement",
            "--history-mode",
            "strict10",
            "--resume",
            "00000000-0000-4000-8000-000000000001",
            "-p",
            "continue",
        ])
        .expect("v0.11 flags");
        assert_eq!(args.phase, Some(PhaseArg::Implement));
        assert_eq!(args.history_mode, Some(HistoryModeArg::Strict10));
        assert_eq!(
            args.resume.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert!(
            Args::try_parse_from([
                "polaris", "--resume", "one", "--fork", "two", "-p", "continue",
            ])
            .is_err()
        );
    }

    #[test]
    fn default_provider_falls_back_to_openai_when_neither_credential_exists() {
        // openai is the arm that owns onboarding; codex's own arm just
        // fails lazily on first use. With nothing saved anywhere, we want
        // onboarding to fire, so this must stay "openai".
        assert_eq!(default_provider_name(false, false), "openai");
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
