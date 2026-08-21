//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

pub mod approver;
pub mod input;
pub mod persist;
pub mod render;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use polaris_core::agent::{self, ToolContext};
use polaris_core::approval::{ApprovalPolicy, Gate};
use polaris_core::audit::AuditLog;
use polaris_core::prompt::AlwaysOn;
use polaris_core::stop::StopTracker;
use polaris_provider::Provider;
use polaris_sandbox::SandboxPolicy;
use polaris_skills::Skill;

use approver::{CrosstermKeyReader, TuiApprover};
use input::{InputAction, apply_key};
use render::{Status, render_chat};

/// Everything `run()` needs, already built by `polaris-cli::main()` the
/// same way the one-shot path builds it. `polaris-tui` never constructs a
/// provider or a sandbox policy itself.
pub struct RunArgs<'a> {
    pub provider: &'a dyn Provider,
    pub state_dir: PathBuf,
    pub audit_path: PathBuf,
    pub max_turns: u32,
    pub sandbox: SandboxPolicy,
    pub helper: PathBuf,
    pub approval_policy: ApprovalPolicy,
    pub always_on: &'a AlwaysOn,
    pub skills: &'a [Skill],
}

pub async fn run(args: RunArgs<'_>) -> ExitCode {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        eprintln!("polaris: refusing to start the TUI on a non-interactive terminal");
        return ExitCode::FAILURE;
    }

    let session_path = args.state_dir.join("tui-session.jsonl");
    let (mut session, truncated) = match persist::load_session(&session_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Can't read {}: {e}", session_path.display());
            return ExitCode::FAILURE;
        }
    };

    let mut audit = match AuditLog::open(&args.audit_path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Can't open the audit log: {e}");
            return ExitCode::FAILURE;
        }
    };

    if truncated {
        eprintln!(
            "warning: {} had a corrupt line; resumed from the messages before it",
            session_path.display()
        );
    }

    let mut terminal = ratatui::init();

    let mut input_buffer = String::new();
    let mut status = Status::Idle;
    let mut key_reader = CrosstermKeyReader;
    let mut fatal_message: Option<String> = None;

    let exit_code = 'outer: loop {
        if terminal
            .draw(|f| render_chat(f, &session, &input_buffer, &status))
            .is_err()
        {
            break ExitCode::FAILURE;
        }

        let event = match ratatui::crossterm::event::read() {
            Ok(e) => e,
            Err(_) => break ExitCode::FAILURE,
        };
        let ratatui::crossterm::event::Event::Key(key) = event else {
            continue;
        };

        let text = match apply_key(&mut input_buffer, key) {
            InputAction::Continue => continue,
            InputAction::Quit => break ExitCode::SUCCESS,
            InputAction::Submit(text) if text.trim().is_empty() => continue,
            InputAction::Submit(text) => text,
        };

        session.push_user(&text);
        let checkpoint = session.messages.len();
        if let Err(e) = persist::append_message(&session_path, session.messages.last().expect("just pushed")) {
            fatal_message = Some(format!("Can't persist the message: {e}"));
            break 'outer ExitCode::FAILURE;
        }

        status = Status::Thinking;
        if terminal
            .draw(|f| render_chat(f, &session, &input_buffer, &status))
            .is_err()
        {
            break ExitCode::FAILURE;
        }

        let mut stop = StopTracker::new(args.max_turns);
        let mut gate = Gate::new(args.approval_policy);
        let mut approver = TuiApprover {
            terminal: &mut terminal,
            reader: &mut key_reader,
        };
        let mut ctx = ToolContext {
            sandbox: &args.sandbox,
            helper: &args.helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        match agent::run(
            args.provider,
            &mut session,
            &mut audit,
            &mut stop,
            args.always_on,
            args.skills,
            &mut ctx,
        )
        .await
        {
            Ok(_) => {
                status = Status::Idle;
                if let Some(reply) = session.messages.last()
                    && let Err(e) = persist::append_message(&session_path, reply)
                {
                    fatal_message = Some(format!("Can't persist the reply: {e}"));
                    break 'outer ExitCode::FAILURE;
                }
            }
            Err(e) => {
                // The agent loop can return after recording an assistant
                // message with tool_calls but before every matching
                // tool-result message is pushed (see AgentError::Stopped /
                // AgentError::Io in polaris_core::agent::run). Re-sending
                // that unbalanced tail to the provider on the next turn
                // would be rejected every time, so roll `session.messages`
                // back to right after the user's message — the only state
                // that was ever actually persisted for this turn.
                session.messages.truncate(checkpoint);
                status = Status::Error(e.to_string());
            }
        }
    };

    ratatui::restore();
    if let Some(msg) = fatal_message {
        eprintln!("{msg}");
    }
    exit_code
}
