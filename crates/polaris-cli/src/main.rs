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
};
use polaris_provider::openai::OpenAiProvider;
use polaris_sandbox::{SandboxMode, SandboxPolicy};

#[derive(Parser)]
#[command(
    name = "polaris",
    about = "A minimal-context coding agent",
    after_help = "\
Environment variables:
  POLARIS_PROVIDER  openai (default) or codex. codex uses the credentials from `polaris login`.
  POLARIS_API_KEY   Required when provider=openai. API key for an OpenAI-compatible endpoint.
  POLARIS_BASE_URL  Used when provider=openai; defaults to https://api.openai.com/v1
  POLARIS_MODEL     Defaults to gpt-5.4 (openai) / gpt-5.6-sol (codex)
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

#[derive(clap::Subcommand)]
enum Command {
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

/// Determines the default model from the stored credentials' `chatgpt_plan_type`
/// claim, the same way `effort_for` determines the default effort — but
/// unlike `effort_for` (called per-request, inside `TokenSource::token`,
/// where a fresh access token is already guaranteed), this runs once at
/// startup before any model string is picked, so it reads the store
/// directly rather than through the async refresh path. A stale-but-still
/// generally-valid stored token is fine here: `chatgpt_plan_type` doesn't
/// change from one refresh to the next, and this is only ever choosing a
/// *default* — an explicit `--model`/`POLARIS_MODEL` always wins over it
/// (see the caller). Returns `None` (defer to `DEFAULT_MODEL`) whenever
/// there's no stored login, the store can't be read, or the plan doesn't
/// map to an override.
fn model_for_stored_plan(store_path: &Path) -> Option<String> {
    let creds = polaris_auth::store::load_from(store_path).ok().flatten()?;
    let plan = polaris_auth::token::plan_type_from_access_token(&creds.access_token);
    polaris_auth::model_for_plan_type(plan.as_deref()).map(|s| s.to_string())
}

/// Same stored-credentials read as `model_for_stored_plan`, but for the
/// TUI footer's displayed effort: seeds `RunArgs::initial_effort_name` so
/// the footer shows the actual plan-derived effort (e.g. "high" for Plus)
/// from the first frame, instead of always starting at
/// `render::DEFAULT_EFFORT` regardless of what's really sent to the API.
/// Returns `None` (defer to `render::DEFAULT_EFFORT`) under the same
/// conditions as `model_for_stored_plan`.
fn effort_for_stored_plan(store_path: &Path) -> Option<String> {
    let creds = polaris_auth::store::load_from(store_path).ok().flatten()?;
    effort_for(&creds.access_token)
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

    match args.command.take() {
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

    let model = std::env::var("POLARIS_MODEL").ok();
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
    // Seeds the TUI footer's displayed effort — `None` means "let it show
    // render::DEFAULT_EFFORT", same as before this existed. Only the codex
    // branch sets this (to the same chatgpt_plan_type-derived value
    // effort_for_stored_plan/effort_for send to the API); the openai
    // branch has no such server-side plan-based effort to reflect.
    let mut initial_effort_name: Option<String> = None;

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
                let model = model.clone().unwrap_or_else(|| "gpt-5.4".to_string());
                model_name = model.clone();
                match OpenAiProvider::new(base, key, model) {
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
                // Precedence: an explicit --model/POLARIS_MODEL always
                // wins; otherwise the stored account's plan may override
                // the default (see model_for_stored_plan's doc comment).
                let model = model
                    .or_else(|| model_for_stored_plan(&store))
                    .unwrap_or_else(|| polaris_provider::codex::DEFAULT_MODEL.to_string());
                model_name = model.clone();
                initial_effort_name = effort_for_stored_plan(&store);
                break std::sync::Arc::new(polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.to_string(),
                    model,
                    std::sync::Arc::new(AuthTokens {
                        issuer: polaris_auth::ISSUER.to_string(),
                        store,
                    }),
                ));
            }
            other => {
                eprintln!("POLARIS_PROVIDER is an unknown value {other}. Specify openai or codex");
                return ExitCode::FAILURE;
            }
        }
    };

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

    let config = polaris_core::config::load(&cwd).unwrap_or_else(|e| {
        eprintln!("Can't read the config: {e}");
        polaris_core::config::Config::default()
    });
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
            let mut session = Session::new();
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
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
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
                sandbox,
                helper,
                approval_policy,
                always_on: &always_on,
                skills: &discovered.skills,
                agent_types: &discovered_agents.agent_types,
                spawn_concurrency: config.spawn_concurrency,
                spawn_write_concurrency: config.spawn_write_concurrency,
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
    fn model_for_stored_plan_defers_when_nothing_is_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = dir.path().join("auth.json");
        assert_eq!(model_for_stored_plan(&store), None);
    }

    #[test]
    fn effort_for_stored_plan_defers_when_nothing_is_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = dir.path().join("auth.json");
        assert_eq!(effort_for_stored_plan(&store), None);
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
