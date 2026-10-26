//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

#[cfg(unix)]
pub mod account;
pub mod approver;
mod auth_browser;
mod clipboard;
#[cfg(unix)]
mod command_dispatch;
#[cfg(test)]
mod continuation_tests;
mod conversation_view;
#[cfg(unix)]
mod draft;
#[cfg(target_os = "macos")]
mod environment_commands;
mod file_input;
#[cfg(target_os = "macos")]
mod file_reveal;
pub mod input;
#[cfg(test)]
mod input_preparation_tests;
#[cfg(all(test, windows))]
mod windows_admission_tests;
#[cfg(windows)]
mod windows_read_only;
#[cfg(windows)]
pub use windows_read_only::run_windows_read_only;
#[cfg(unix)]
mod instruction_commands;
#[cfg(unix)]
mod local_actions;
#[cfg(unix)]
mod mcp_commands;
mod memory;
#[cfg(unix)]
mod memory_commands;
pub mod onboarding;
pub mod persist;
#[cfg(target_os = "linux")]
mod resume_effect;
#[cfg(unix)]
mod role_commands;
#[cfg(unix)]
mod runtime_commands;
#[cfg(unix)]
mod runtime_effects;
#[cfg(unix)]
mod sandbox_commands;
mod session_activation;
#[cfg(unix)]
mod session_commands;
#[cfg(target_os = "linux")]
mod workspace_commands;
pub use memory::{configure_archive, configure_memory, lock_memory};
pub mod render;
mod rewind_ui;
mod screen_selection;
mod selection;
pub mod sessions;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod signal;
pub mod slash;
#[cfg(target_os = "linux")]
mod source_apply;
mod status_view;
mod terminal;
#[cfg(test)]
use status_view::status_row;
use status_view::{StatusSnapshot, status_lines};
mod time;
pub mod tool_memory;

use std::cell::RefCell;
use std::collections::VecDeque;
#[cfg(unix)]
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use polaris_core::agent::{self, ToolContext};
use polaris_core::approval::{ApprovalPolicy, Gate};
use polaris_core::audit::AuditLog;
use polaris_core::prompt::AlwaysOn;
use polaris_core::session::{RootDelegationConfigV1, RootDelegationControlV1};
use polaris_core::stop::StopTracker;
use polaris_core::subagent_control::{
    ApprovalCapabilityV1, CapabilitySnapshotV1, McpCapabilityV1, RootAttemptId, RootRunId,
    SandboxCapabilityV1, TaskId,
};
use polaris_provider::Provider;
use polaris_provider::local::{Capability, Endpoint, LocalAdapter, ModelMetadata, Runtime};
use polaris_sandbox::SandboxPolicy;
use polaris_skills::Skill;
use ratatui::backend::Backend;
use zeroize::Zeroize;

#[cfg(not(target_os = "linux"))]
use approver::CrosstermKeyReader;
use approver::TuiApprover;
use input::{InputAction, apply_key};
use render::Status;
use session_activation::{
    MAX_TOOL_MEMORY_ORIGINS, ProcessServices, apply_compaction_and_memory, apply_tool_memory,
    attach_process_services, configure_saved_tool_memory, saved_tool_memory_namespace,
};
#[cfg(unix)]
use session_activation::{
    SavedDraftTarget, apply_saved_binding, apply_saved_draft, read_saved_activation,
};

const UNDECIDED_EFFORT: &str = "undecided";

#[cfg(target_os = "linux")]
const LINUX_SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(target_os = "linux")]
const LINUX_SHUTDOWN_DURING_RESTORE: &str = "Stop request received";

#[cfg(target_os = "linux")]
fn linux_signal_exit_code(reason: signal::LinuxProcessShutdown) -> ExitCode {
    match reason {
        signal::LinuxProcessShutdown::SigInt => ExitCode::from(130),
        signal::LinuxProcessShutdown::SigTerm => ExitCode::from(143),
    }
}

#[cfg(target_os = "linux")]
fn linux_pending_exit_code() -> Option<ExitCode> {
    signal::pending_linux_process_shutdown().map(linux_signal_exit_code)
}

#[cfg(target_os = "linux")]
fn linux_exit_after_terminal_restoration(
    requested: ExitCode,
    restoration: std::io::Result<()>,
    pending: Option<signal::LinuxProcessShutdown>,
) -> (ExitCode, Option<String>) {
    match restoration {
        Err(error) => (
            ExitCode::FAILURE,
            Some(format!("terminal restoration failed: {error}")),
        ),
        Ok(()) if requested == ExitCode::SUCCESS => (
            pending.map(linux_signal_exit_code).unwrap_or(requested),
            None,
        ),
        Ok(()) => (requested, None),
    }
}

#[cfg(target_os = "linux")]
fn linux_cancel_owned_children(
    shutdown: Option<signal::LinuxProcessShutdown>,
    turn_completed: bool,
) -> bool {
    shutdown.is_some() || !turn_completed
}

#[cfg(target_os = "linux")]
fn linux_admission_status_after_cleanup(
    status: polaris_core::session_manager::TerminalStatus,
    shutdown: Option<signal::LinuxProcessShutdown>,
) -> polaris_core::session_manager::TerminalStatus {
    if shutdown.is_some() {
        polaris_core::session_manager::TerminalStatus::OutcomeUnknown
    } else {
        status
    }
}

#[cfg(target_os = "linux")]
fn linux_turn_completed_without_shutdown(
    turn_completed: bool,
    shutdown: Option<signal::LinuxProcessShutdown>,
) -> bool {
    turn_completed && shutdown.is_none()
}

// Admission has already made the input durable, but no agent future or root
// control exists yet. Close each owned component and leave an explicit
// unknown terminal receipt before taking the signal exit.
#[cfg(target_os = "linux")]
async fn linux_finish_pre_turn_shutdown(
    admission: Option<TuiAdmissionTurn>,
    runtime: Option<polaris_core::mcp::runtime::RunRuntime>,
    boundary: Option<polaris_core::conversation_state::RewindBoundaryV1>,
    persistence: Option<&polaris_core::session_store::PersistedSession>,
) -> Result<(), String> {
    let mut first_error = None;
    if let Some(mut runtime) = runtime
        && let Err(error) = runtime.shutdown().await
    {
        first_error = Some(format!("Cannot stop the MCP runtime: {error}"));
    }
    if let Some(admission) = admission
        && let Err(error) =
            admission.finish(polaris_core::session_manager::TerminalStatus::OutcomeUnknown)
        && first_error.is_none()
    {
        first_error = Some(format!("Cannot stop the sidechat run: {error}"));
    }
    if let Some(boundary) = boundary {
        let result = persistence
            .ok_or_else(|| "Rewind boundary storage source was lost".to_string())
            .and_then(|saved| {
                saved
                    .abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "The turn was not started because a stop request was received".into(),
                    )
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = result
            && first_error.is_none()
        {
            first_error = Some(format!("Cannot finalize the rewind boundary: {error}"));
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(target_os = "linux")]
enum LinuxTerminalStartError {
    Shutdown(signal::LinuxProcessShutdown),
    Failed(String),
}

// Check at the terminal boundary, before entering raw or alternate-screen
// mode. Keeping the check outside the initializer also lets the unit test
// prove that a pre-raw signal made no terminal call at all.
#[cfg(target_os = "linux")]
fn linux_start_terminal<T>(
    observe: impl FnOnce() -> Option<signal::LinuxProcessShutdown>,
    start: impl FnOnce() -> Result<T, String>,
) -> Result<T, LinuxTerminalStartError> {
    if let Some(reason) = observe() {
        return Err(LinuxTerminalStartError::Shutdown(reason));
    }
    start().map_err(LinuxTerminalStartError::Failed)
}

#[cfg(any(target_os = "linux", windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxTerminalStep {
    Cursor,
    Mouse,
    Raw,
    Alternate,
}

// Each lifetime owns only the state it successfully acquired. Even if one
// restoration fails, the other acquired states are attempted once.
#[cfg(any(target_os = "linux", windows))]
#[derive(Default)]
struct LinuxTerminalGuard {
    alternate: bool,
    raw: bool,
    mouse: bool,
    cursor: bool,
}

#[cfg(any(target_os = "linux", windows))]
impl LinuxTerminalGuard {
    #[cfg(target_os = "linux")]
    fn enter() -> Result<(Self, crate::terminal::DefaultTerminal), String> {
        let mut guard = Self::default();
        ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::terminal::EnterAlternateScreen
        )
        .map_err(|error| format!("Can't enter the alternate screen: {error}"))?;
        guard.alternate = true;
        if let Err(error) = ratatui::crossterm::terminal::enable_raw_mode() {
            return Err(guard.start_failed(format!("Can't enable raw mode: {error}")));
        }
        guard.raw = true;
        let terminal = ratatui::Terminal::with_options(
            crate::terminal::backend(),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fullscreen,
            },
        )
        .map_err(|error| guard.start_failed(format!("Can't initialize terminal: {error}")))?;
        Ok((guard, terminal))
    }

    #[cfg(target_os = "linux")]
    fn start_failed(&mut self, error: String) -> String {
        match self.restore() {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; terminal restoration failed: {cleanup}"),
        }
    }

    fn enable_mouse_and_cursor(&mut self) {
        // Restore both modes even if enabling them only partially succeeds.
        self.mouse = true;
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::EnableMouseCapture,
            ratatui::crossterm::event::EnableBracketedPaste
        );
        if ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::cursor::SetCursorStyle::BlinkingBar
        )
        .is_ok()
        {
            self.cursor = true;
        }
    }

    fn restore_with(
        &mut self,
        mut restore_step: impl FnMut(LinuxTerminalStep) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut first_error = None;
        for (step, acquired) in [
            (LinuxTerminalStep::Cursor, &mut self.cursor),
            (LinuxTerminalStep::Mouse, &mut self.mouse),
            (LinuxTerminalStep::Raw, &mut self.raw),
            (LinuxTerminalStep::Alternate, &mut self.alternate),
        ] {
            if std::mem::take(acquired)
                && let Err(error) = restore_step(step)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn restore(&mut self) -> std::io::Result<()> {
        self.restore_with(linux_restore_terminal_step)
    }
}

#[cfg(any(target_os = "linux", windows))]
fn linux_restore_terminal_step(step: LinuxTerminalStep) -> std::io::Result<()> {
    match step {
        LinuxTerminalStep::Cursor => ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
        ),
        LinuxTerminalStep::Mouse => ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::DisableBracketedPaste,
            ratatui::crossterm::event::DisableMouseCapture
        ),
        LinuxTerminalStep::Raw => ratatui::crossterm::terminal::disable_raw_mode(),
        LinuxTerminalStep::Alternate => ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::terminal::LeaveAlternateScreen
        ),
    }
}

#[cfg(any(target_os = "linux", windows))]
fn linux_finish_guarded_terminal<T>(
    operation: Result<T, String>,
    guard: &mut LinuxTerminalGuard,
    restore_step: impl FnMut(LinuxTerminalStep) -> std::io::Result<()>,
) -> Result<T, String> {
    let restoration = guard
        .restore_with(restore_step)
        .map_err(|error| error.to_string());
    match (operation, restoration) {
        (Err(error), Err(cleanup)) => {
            Err(format!("{error}; terminal restoration failed: {cleanup}"))
        }
        (Err(error), _) => Err(error),
        (_, Err(cleanup)) => Err(format!("terminal restoration failed: {cleanup}")),
        (Ok(value), Ok(())) => Ok(value),
    }
}

#[cfg(any(target_os = "linux", windows))]
impl Drop for LinuxTerminalGuard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("terminal restoration failed: {error}");
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_read_key_with(
    mut poll: impl FnMut(Duration) -> std::io::Result<bool>,
    mut read: impl FnMut() -> std::io::Result<ratatui::crossterm::event::Event>,
    mut shutdown_pending: impl FnMut() -> bool,
) -> std::io::Result<ratatui::crossterm::event::KeyCode> {
    use ratatui::crossterm::event::{Event, KeyEventKind};
    loop {
        if shutdown_pending() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        if !poll(LINUX_SIGNAL_POLL_INTERVAL)? {
            continue;
        }
        if shutdown_pending() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        if let Event::Key(key) = read()?
            && key.kind == KeyEventKind::Press
        {
            if shutdown_pending() {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            return Ok(key.code);
        }
    }
}

#[cfg(target_os = "linux")]
struct LinuxSignalAwareKeyReader;

#[cfg(target_os = "linux")]
impl approver::KeyReader for LinuxSignalAwareKeyReader {
    fn read_key(&mut self) -> std::io::Result<ratatui::crossterm::event::KeyCode> {
        linux_read_key_with(
            ratatui::crossterm::event::poll,
            ratatui::crossterm::event::read,
            || signal::pending_linux_process_shutdown().is_some(),
        )
    }
}

#[cfg(target_os = "linux")]
fn linux_read_idle_event_with(
    mut poll: impl FnMut(Duration) -> std::io::Result<bool>,
    mut read: impl FnMut() -> std::io::Result<ratatui::crossterm::event::Event>,
    mut shutdown_pending: impl FnMut() -> bool,
) -> std::io::Result<Option<ratatui::crossterm::event::Event>> {
    loop {
        if shutdown_pending() {
            return Ok(None);
        }
        if !poll(LINUX_SIGNAL_POLL_INTERVAL)? {
            continue;
        }
        if shutdown_pending() {
            return Ok(None);
        }
        let event = read()?;
        if shutdown_pending() {
            return Ok(None);
        }
        return Ok(Some(event));
    }
}

/// CLI-owned constructor, invoked only when a selected strict session needs it.
pub type HistoryBackend = (
    Arc<polaris_core::conversation_memory::StrictHistory>,
    polaris_provider::UsageMeter,
);
pub type HistoryBackendFactory = Arc<dyn Fn() -> Result<HistoryBackend, String> + Send + Sync>;
pub type TurnAuthorityInstaller =
    Arc<dyn Fn(&mut polaris_core::session::Session, &str) -> Result<(), String> + Send + Sync>;

#[cfg(unix)]
fn active_saved_session_id(
    session: &polaris_core::session::Session,
) -> Result<Option<String>, String> {
    session.check_persistence().map_err(|e| e.to_string())?;
    session
        .persistence
        .as_ref()
        .map(|p| p.snapshot().map(|snapshot| snapshot.state.session_id))
        .transpose()
        .map_err(|e| e.to_string())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn handle_runtime_command(
    request: &str,
    control: Option<&Arc<RootDelegationControlV1>>,
    saved: Option<&polaris_core::session_store::PersistedSession>,
    state_dir: &Path,
    roles: &[polaris_skills::AgentType],
    sandbox: &SandboxPolicy,
    approval: ApprovalPolicy,
    next_max_turns: &mut u32,
    startup_max_turns: u32,
    activity: &conversation_view::ConversationViewState,
) -> Result<Vec<String>, String> {
    let (verb, extra) = request.split_once(' ').unwrap_or((request, ""));
    match verb {
        "subagents" => runtime_commands::subagents(control, extra, None).await,
        "roles" => runtime_commands::roles(roles, extra),
        "run" if extra == "details" => {
            runtime_commands::run_details(control)
                .await
                .map(|mut lines| {
                    lines.extend(activity.tool_detail_lines());
                    lines
                })
        }
        "run" => runtime_commands::run_settings(extra, next_max_turns, startup_max_turns),
        "sandbox" => runtime_commands::sandbox(extra, sandbox, approval),
        "input" => {
            let words = extra.split_whitespace().collect::<Vec<_>>();
            let ["status", id] = words.as_slice() else {
                return Err(
                    "/input status <admission-id>. Press Esc to cancel a running task.".into(),
                );
            };
            let saved = saved.ok_or("Admission records require a v2 conversation")?;
            let snapshot = saved.snapshot().map_err(|e| e.to_string())?;
            let current = polaris_core::session_manager::SessionId::new(snapshot.state.session_id)
                .map_err(|e| e.to_string())?;
            if !state_dir.join("tui-admissions-v1").exists() {
                return Err("No admission records saved in this conversation".into());
            }
            let manager = polaris_core::desktop_store::SessionManagerStore::inspect_family_owned(
                &state_dir.join("tui-admissions-v1"),
                &current,
            )
            .map_err(|e| e.to_string())?
            .ok_or("No admission records saved in this conversation")?;
            let id =
                polaris_core::session_manager::AdmissionId::new(*id).map_err(|e| e.to_string())?;
            let admitted = manager.status(&id).map_err(|e| e.to_string())?;
            use polaris_core::session_manager::AdmissionState;
            let destination = match &admitted.state {
                AdmissionState::DestinationReady { session_id, .. }
                | AdmissionState::InputAppended { session_id }
                | AdmissionState::RunQueued { session_id, .. }
                | AdmissionState::Running { session_id, .. }
                | AdmissionState::Terminal {
                    session_id: Some(session_id),
                    ..
                } => Some(session_id),
                _ => None,
            };
            if admitted.parent_session != current && destination != Some(&current) {
                return Err("This admission record does not belong to this conversation".into());
            }
            Ok(vec![render::sanitize(&format!(
                "Input {} / revision {} / state {:?}",
                admitted.id.as_str(),
                admitted.revision,
                admitted.state
            ))])
        }
        "instruction" => Err(
            "The persistent owner of the running instruction is connected. Use normal input".into(),
        ),
        _ => Err(
            "Specify /subagents, /roles, /run settings|details, /sandbox, or /input status".into(),
        ),
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn handle_queued_control<B: ratatui::backend::Backend, R: approver::KeyReader>(
    request: &str,
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    session: &polaris_core::session::Session,
    workspace: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    state_dir: &Path,
    queue: &mut VecDeque<instruction_commands::QueuedInstructionV1>,
    active: &mut Option<instruction_commands::QueuedInstructionV1>,
) -> Option<Result<Vec<String>, String>> {
    let receive = request.strip_prefix("instruction send ");
    let cancel = request.strip_prefix("input cancel ");
    if receive.is_none() && cancel.is_none() {
        return None;
    }
    Some((|| {
        let saved = session
            .persistence
            .as_ref()
            .ok_or("A saved v2 conversation is required")?;
        let snapshot = saved.snapshot().map_err(|e| e.to_string())?;
        let current =
            polaris_core::session_manager::SessionId::new(snapshot.state.session_id.clone())
                .map_err(|e| e.to_string())?;
        let activity = workspace
            .try_activity_quiescent()
            .map_err(|e| e.to_string())?;
        let (store, mut manager) = open_tui_admission_owner(state_dir, &current)?;
        if let Some(text) = receive {
            let handle = instruction_commands::receive(
                &store,
                &mut manager,
                saved,
                effects,
                &activity,
                polaris_core::management_effect::ManagementOperationIdV1::random()
                    .map_err(|e| e.to_string())?,
                text,
            )?;
            let id = handle.admission_id().as_str().to_owned();
            queue.push_back(handle);
            return Ok(vec![format!(
                "Instruction admission ID: {id}. Applied at the next run boundary"
            )]);
        }
        let id = polaris_core::session_manager::AdmissionId::new(cancel.expect("cancel"))
            .map_err(|e| e.to_string())?;
        let position = queue.iter().position(|item| item.admission_id() == &id);
        let is_active = active
            .as_ref()
            .is_some_and(|item| item.admission_id() == &id);
        if position.is_none() && !is_active {
            return Err("This is not a queued input ID for this conversation".into());
        }
        if !account::confirm(
            terminal,
            reader,
            "Cancel queued input",
            id.as_str(),
            "Cancel only the unexecuted instruction with this ID.",
        ) {
            let context = runtime_commands::RuntimeMutationContext {
                effects,
                lease: &activity,
                session_id: &snapshot.state.session_id,
                generation: snapshot.state.generation,
                accepted: false,
            };
            runtime_effects::begin(&context, "D10", id.as_str().as_bytes())?;
            return Ok(vec!["Cancellation request cancelled".into()]);
        }
        let handle = if is_active {
            active.take().expect("active")
        } else {
            queue.remove(position.expect("position")).expect("queued")
        };
        instruction_commands::cancel_queue(
            &store,
            &mut manager,
            saved,
            effects,
            &activity,
            handle,
        )?;
        Ok(vec![format!("Input {} cancelled", id.as_str())])
    })())
}

/// Everything `run()` needs, already built by `polaris-cli::main()` the
/// same way the one-shot path builds it. `polaris-tui` never constructs a
/// provider or a sandbox policy itself.
pub struct RunArgs<'a> {
    #[cfg(unix)]
    pub account_binding: Option<Arc<dyn account::BindingControl>>,
    /// Shared rather than borrowed: `agent::run` needs both a `&dyn
    /// Provider` for the root's own turns and an owned handle it can hand
    /// to a wave of subagents (see `agent::run`'s `provider_pool`).
    pub provider: Arc<dyn Provider>,
    /// A fixed independent selector; model-picker changes affect only the primary.
    pub mcp_selection_provider: Option<Arc<dyn Provider>>,
    /// The same typed judge and tier selector used by one-shot runs. TUI
    /// sessions retain them across `/new`, `/fork`, and `/resume`.
    pub decider: Option<Arc<dyn polaris_provider::decision::Decider>>,
    pub decision_configuration: polaris_core::config::DecisionConfig,
    pub decision_colocation: polaris_core::decision_capacity::colocation::ColocationSources,
    pub tier_decider: Option<Arc<dyn polaris_core::tier::TierDecider>>,
    /// Non-authoritative final-answer observer. It runs only after the
    /// canonical assistant message is durable and can never replace output.
    pub answer_finalizer: Option<Arc<polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>>,
    pub sidechat_auto: bool,
    /// Host-visible root tool capability names. The TUI does not discover a
    /// second catalogue; the CLI passes the same fixed list as one-shot runs.
    pub tool_names: Vec<String>,
    /// Optional host-issued authority bound to the first ordinary TUI input.
    /// The installer owns validation and single-use semantics; model text
    /// cannot construct or retain it.
    pub turn_authority: Option<TurnAuthorityInstaller>,
    pub provider_name: String,
    pub model_name: String,
    /// Seeds the footer with the caller's selected effort, as a wire token
    /// (`--effort`'s own spelling); it is translated to the matching
    /// catalog row by `render::effort_display_name`. Absent means Jev judges
    /// effort for each hosted turn and the footer starts as `undecided`.
    pub initial_effort_name: Option<String>,
    pub cwd: PathBuf,
    pub state_dir: PathBuf,
    /// Where every saved conversation lives, across every project —
    /// `~/.polaris/sessions/` (see `polaris_core::project::sessions_dir`).
    /// Distinct from `state_dir`, which stays per-project (audit log,
    /// sandbox staging).
    pub sessions_dir: PathBuf,
    pub audit_path: PathBuf,
    pub max_turns: u32,
    pub remember: bool,
    pub tool_memory: Option<polaris_core::tool_memory::RetentionMode>,
    pub tool_memory_embedding: Option<(String, String)>,
    pub compact_at: Option<usize>,
    pub sandbox: SandboxPolicy,
    pub helper: PathBuf,
    pub approval_policy: ApprovalPolicy,
    /// Explicit permission for MCP tool calls and registration; each still asks.
    pub mcp_external: bool,
    pub always_on: &'a AlwaysOn,
    pub skills: &'a [Skill],
    /// The subagent types `spawn` can resolve a task against.
    pub agent_types: &'a [polaris_skills::AgentType],
    /// How many `spawn` tasks a single wave may run concurrently, and how
    /// many of those may hold a `write_root` at once — from
    /// `Config::spawn_concurrency` / `Config::spawn_write_concurrency`
    /// (see `polaris_core::config`).
    pub spawn_concurrency: usize,
    pub spawn_write_concurrency: usize,
    /// Root-only automatic files.md regeneration, inherited by every TUI session reset.
    pub files_md_auto_regenerate: bool,
    /// How much of `--approval always`'s band the person agreed in advance
    /// to stop being asked about, from `[approval] auto` (M18).
    pub approval_auto: polaris_core::approval::AutoApproval,
    /// Disabled configurations preserve legacy sessions by leaving workflow
    /// state absent.
    pub workflow_config: polaris_core::workflow::WorkflowConfig,
    /// CLI input can override the configured initial phase for this launch.
    pub initial_phase: Option<polaris_core::workflow::Phase>,
    pub history_mode: polaris_core::conversation_state::HistoryMode,
    /// Frozen at root-session creation from top-level `input_preparation`.
    pub input_preparation: polaris_core::input_preparation::InputPreparationMode,
    /// CLI has already resolved this target against the selected project.  The
    /// TUI must not retry that lookup or fall back to a fresh session.
    pub continuation: Option<PreparedContinuation>,
    /// Only an explicit CLI reconfiguration may replace the saved mode.
    pub workflow_mode_overridden: bool,
    pub strict_history: Option<Arc<polaris_core::conversation_memory::StrictHistory>>,
    pub summary_usage: Option<polaris_provider::UsageMeter>,
    pub history_backend_factory: Option<HistoryBackendFactory>,
}

/// A durable session selected by the CLI before entering the interactive UI.
/// The distinction is retained because only a resume performs strict-history
/// recovery; a fork has already made its independent child durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationKind {
    Resume,
    Fork,
}

#[derive(Clone)]
pub struct PreparedContinuation {
    pub kind: ContinuationKind,
    pub persistence: polaris_core::session_store::PersistedSession,
}

struct WorkflowSessionArgs<'a> {
    config: &'a polaris_core::workflow::WorkflowConfig,
    initial_phase: Option<polaris_core::workflow::Phase>,
    history_mode: polaris_core::conversation_state::HistoryMode,
    input_preparation: polaris_core::input_preparation::InputPreparationMode,
    cwd: &'a std::path::Path,
    state_dir: &'a std::path::Path,
    sessions_dir: &'a std::path::Path,
    strict_history: Option<Arc<polaris_core::conversation_memory::StrictHistory>>,
    summary_usage: Option<polaris_provider::UsageMeter>,
    history_backend_factory: Option<&'a HistoryBackendFactory>,
    files_md_auto_regenerate: bool,
    approval_auto: polaris_core::approval::AutoApproval,
    continuation: Option<PreparedContinuation>,
    workflow_mode_overridden: bool,
    skills: &'a [Skill],
    #[cfg(unix)]
    management_effects: Option<&'a polaris_core::management_effect::ManagementEffectStoreV1>,
    #[cfg(unix)]
    project_mutation: Option<&'a polaris_core::rewind_workspace::WorkspaceOwner>,
}

fn ensure_history_backend(
    session: &mut polaris_core::session::Session,
    mode: polaris_core::conversation_state::HistoryMode,
    factory: Option<&HistoryBackendFactory>,
) -> Result<(), String> {
    use polaris_core::conversation_state::HistoryMode;
    if matches!(mode, HistoryMode::Strict10 | HistoryMode::Structured10)
        && session.strict_history.is_none()
    {
        let (history, usage) = factory.ok_or("No history embedding or summarization backend")?()?;
        session.strict_history = Some(history);
        session.summary_usage = Some(usage);
    }
    Ok(())
}

/// Recover saved responses before workflow checkpoints. Replace the visible
/// session only after recovery and explicit configuration validation succeed.
pub async fn restore_prepared_continuation(
    session: &mut polaris_core::session::Session,
    continuation: &PreparedContinuation,
    config: &polaris_core::workflow::WorkflowConfig,
    workflow_mode_overridden: bool,
    skills: &[Skill],
    history_backend_factory: Option<&HistoryBackendFactory>,
) -> Result<(), String> {
    restore_prepared_continuation_inner(
        session,
        continuation,
        config,
        workflow_mode_overridden,
        true,
        skills,
        history_backend_factory,
    )
    .await
    .map(|_| ())
}

/// Restore history for an F15 candidate without an unowned workflow write.
/// The caller confirms and records F15 through the management owner afterward.
pub async fn restore_prepared_continuation_for_managed_override(
    session: &mut polaris_core::session::Session,
    continuation: &PreparedContinuation,
    config: &polaris_core::workflow::WorkflowConfig,
    skills: &[Skill],
    history_backend_factory: Option<&HistoryBackendFactory>,
) -> Result<(), String> {
    restore_prepared_continuation_inner(
        session,
        continuation,
        config,
        false,
        false,
        skills,
        history_backend_factory,
    )
    .await
    .map(|_| ())
}

struct ContinuationWorkflowPublication {
    #[cfg(target_os = "linux")]
    before: polaris_core::conversation_state::ConversationSnapshot,
    #[cfg(target_os = "linux")]
    published: polaris_core::conversation_state::ConversationSnapshot,
}

async fn restore_prepared_continuation_inner(
    session: &mut polaris_core::session::Session,
    continuation: &PreparedContinuation,
    config: &polaris_core::workflow::WorkflowConfig,
    workflow_mode_overridden: bool,
    allow_compat_checkpoint: bool,
    skills: &[Skill],
    history_backend_factory: Option<&HistoryBackendFactory>,
) -> Result<Option<ContinuationWorkflowPublication>, String> {
    let mut candidate_session = session.clone();
    let mode = continuation
        .persistence
        .snapshot()
        .map_err(|error| error.to_string())?
        .state
        .history_mode;
    ensure_history_backend(&mut candidate_session, mode, history_backend_factory)?;
    candidate_session
        .attach(continuation.persistence.clone())
        .map_err(|error| format!("Cannot attach the v2 session: {error}"))?;
    candidate_session
        .recover_saved_history()
        .await
        .map_err(|error| format!("Cannot restore saved history: {error}"))?;
    #[cfg(target_os = "linux")]
    if signal::pending_linux_process_shutdown().is_some() {
        return Err(LINUX_SHUTDOWN_DURING_RESTORE.into());
    }
    let before_workflow = continuation
        .persistence
        .snapshot()
        .map_err(|error| error.to_string())?;
    let (restored, needs_checkpoint) = continuation
        .persistence
        .prepare_workflow(config.clone())
        .map_err(|error| format!("Cannot restore workflow state: {error}"))?;
    let workflow = if workflow_mode_overridden {
        let mut candidate_config = restored.config.clone();
        candidate_config.enabled = config.enabled;
        candidate_config.mode = config.mode;
        let candidate = polaris_core::workflow::SessionWorkflow::restore(
            candidate_config,
            restored.gates.clone(),
            restored.state(),
        )
        .map_err(|error| format!("Cannot verify workflow settings: {error}"))?;
        if candidate.config.enabled {
            drop(
                candidate
                    .begin_turn_from_disk(skills)
                    .map_err(|error| format!("Cannot verify workflow settings: {error}"))?,
            );
        }
        candidate
    } else {
        restored
    };
    if workflow_mode_overridden {
        return Err(
            "Workflow changes require management operation confirmation and recording".into(),
        );
    }
    let publication = if needs_checkpoint {
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return Err(LINUX_SHUTDOWN_DURING_RESTORE.into());
        }
        if !allow_compat_checkpoint {
            return Err(
                "Migrate workflow compatibility state after recovering management operations"
                    .into(),
            );
        }
        let mut candidate = before_workflow.state.clone();
        candidate.workflow_config = Some(
            polaris_core::conversation_state::SavedWorkflowConfigV1::capture(&workflow.config),
        );
        candidate.workflow = workflow.state();
        candidate.gates = workflow.gates.clone();
        candidate.workflow_artifacts = workflow.artifact_graph().clone();
        let before_hash = polaris_core::conversation_state::content_hash(
            &serde_json::to_vec(&before_workflow.state).map_err(|error| error.to_string())?,
        );
        let published = continuation
            .persistence
            .publish_workflow_change(before_workflow.state.generation, &before_hash, candidate)
            .map_err(|error| format!("Cannot save workflow settings: {error}"))?;
        #[cfg(not(target_os = "linux"))]
        drop(published);
        Some(ContinuationWorkflowPublication {
            #[cfg(target_os = "linux")]
            before: before_workflow,
            #[cfg(target_os = "linux")]
            published,
        })
    } else {
        None
    };
    candidate_session.workflow = Some(workflow);
    *session = candidate_session;
    Ok(publication)
}

async fn new_workflow_session(
    args: WorkflowSessionArgs<'_>,
) -> Result<polaris_core::session::Session, String> {
    require_turn_owner()?;
    let WorkflowSessionArgs {
        config,
        initial_phase,
        history_mode,
        input_preparation,
        cwd,
        state_dir,
        sessions_dir,
        strict_history,
        summary_usage,
        history_backend_factory,
        files_md_auto_regenerate,
        approval_auto,
        continuation,
        workflow_mode_overridden,
        skills,
        #[cfg(unix)]
        management_effects,
        #[cfg(unix)]
        project_mutation,
    } = args;
    let mut session = polaris_core::session::Session::new();
    session.input_preparation = input_preparation;
    session.disable_files_md_auto_regenerate = !files_md_auto_regenerate;
    session.approval_auto = approval_auto;
    session.strict_history = strict_history;
    session.summary_usage = summary_usage;
    let has_continuation = continuation.is_some();
    if let Some(continuation) = continuation {
        if workflow_mode_overridden {
            restore_prepared_continuation_for_managed_override(
                &mut session,
                &continuation,
                config,
                skills,
                history_backend_factory,
            )
            .await?;
        } else {
            restore_prepared_continuation(
                &mut session,
                &continuation,
                config,
                false,
                skills,
                history_backend_factory,
            )
            .await?;
        }
    } else if config.enabled {
        let workflow = polaris_core::workflow::SessionWorkflow::try_new(config.clone())?;
        if let Some(phase) = initial_phase {
            workflow.request_phase(phase)?;
        }
        session.workflow = Some(workflow);
    } else if initial_phase.is_some() {
        return Err("--phase requires [workflow].enabled = true in configuration".into());
    }
    // Production Unix input always needs the v2 admission owner, including
    // workflow opt-out with legacy history. Isolated caller tests can still
    // exercise the older JSONL lifecycle without a management store.
    #[cfg(unix)]
    let managed_fresh = management_effects.is_some();
    #[cfg(not(unix))]
    let managed_fresh = false;
    if !has_continuation
        && (managed_fresh
            || config.enabled
            || history_mode != polaris_core::conversation_state::HistoryMode::Legacy)
    {
        ensure_history_backend(&mut session, history_mode, history_backend_factory)?;
        let data_root = sessions_dir
            .parent()
            .ok_or("sessions directory has no data root")?;
        // The identity is the project's, not the launch directory's — the
        // same convention `memory.rs` and `tool_memory.rs` already follow.
        // Without the resolve, starting from a subdirectory files the
        // session under a second identity the CLI never looks in.
        let project_id = persist::project_identity(&polaris_core::project::resolve_root(cwd))
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        let persistence = if let Some(effects) = management_effects {
            let owner = project_mutation.ok_or("project mutation owner is unavailable")?;
            let lease = owner.try_mutation().map_err(|error| error.to_string())?;
            polaris_core::session_switch::SessionSwitchOwner {
                effects,
                lease: &lease,
                state_dir,
            }
            .create_new(
                None,
                data_root,
                &state_dir.join("memory.sqlite3"),
                &project_id,
                history_mode,
                session.workflow.as_ref(),
                polaris_core::management_effect::ManagementInventoryIdV1::parse("T06")
                    .expect("approved T06 inventory"),
            )
            .map_err(|error| error.to_string())?
        } else {
            polaris_core::session_store::PersistedSession::create(
                data_root,
                &state_dir.join("memory.sqlite3"),
                &project_id,
                history_mode,
                session.workflow.as_ref(),
            )
            .map_err(|error| error.to_string())?
        };
        #[cfg(not(unix))]
        let persistence = polaris_core::session_store::PersistedSession::create(
            data_root,
            &state_dir.join("memory.sqlite3"),
            &project_id,
            history_mode,
            session.workflow.as_ref(),
        )
        .map_err(|error| error.to_string())?;
        session
            .attach(persistence)
            .map_err(|error| error.to_string())?;
    }
    Ok(session)
}

fn delegation_capabilities(
    sandbox: &SandboxPolicy,
    approval: ApprovalPolicy,
    mcp_available: bool,
    tool_names: &[String],
) -> CapabilitySnapshotV1 {
    let sandbox_capability = match sandbox.mode() {
        polaris_sandbox::SandboxMode::ReadOnly => SandboxCapabilityV1::ReadOnly,
        polaris_sandbox::SandboxMode::WorkspaceWrite => SandboxCapabilityV1::WorkspaceWrite,
        polaris_sandbox::SandboxMode::FullAccess => SandboxCapabilityV1::FullAccess,
    };
    let approval = match approval {
        ApprovalPolicy::Never => ApprovalCapabilityV1::Never,
        ApprovalPolicy::OnRequest => ApprovalCapabilityV1::OnRequest,
        ApprovalPolicy::Always => ApprovalCapabilityV1::TrustedOwnerOnly,
    };
    CapabilitySnapshotV1 {
        sandbox: sandbox_capability,
        write_roots: sandbox
            .writable_roots()
            .iter()
            .filter_map(|path| path.to_str().map(str::to_owned))
            .collect(),
        tool_names: tool_names.to_vec(),
        approval,
        mcp: if mcp_available {
            McpCapabilityV1::ReadSnapshotsOnly
        } else {
            McpCapabilityV1::None
        },
    }
}

struct TuiAdmissionTurn {
    store: polaris_core::desktop_store::SessionManagerStore,
    manager: polaris_core::session_manager::SessionManagerV1,
    admission_id: polaris_core::session_manager::AdmissionId,
}

struct TuiPromptRequest<'a> {
    host: &'a polaris_core::mcp::local::LocalHost,
    choice: &'a polaris_core::mcp::runtime::McpPromptChoice,
    selector: Option<Arc<dyn Provider>>,
    ceiling: polaris_core::mcp::operation::ExternalCeiling,
}

async fn select_tui_prompt(
    request: TuiPromptRequest<'_>,
    session_id: &str,
    run_id: &str,
) -> Result<
    (
        polaris_core::mcp::runtime::RunRuntime,
        polaris_provider::Message,
    ),
    (String, bool),
> {
    let (mut runtime, _) = request
        .host
        .runtime(session_id, run_id, request.ceiling)
        .map_err(|error| (format!("Cannot prepare MCP: {error}"), false))?
        .ok_or_else(|| ("MCP servers are unavailable".to_owned(), false))?;
    request
        .host
        .configure_preselection(&mut runtime, request.selector, None);
    let selected = tokio::time::timeout(
        Duration::from_secs(30),
        runtime.host_prompt(request.choice, std::future::pending()),
    )
    .await;
    match selected {
        Ok(Ok(result)) if result.permits_continuation() => Ok((runtime, result.message)),
        Ok(Ok(result)) => {
            let unknown =
                result.disposition == polaris_core::desktop_store::McpDisposition::OutcomeUnknown;
            let _ = runtime.shutdown().await;
            Err(("Cannot verify the MCP prompt".into(), unknown))
        }
        Ok(Err(error)) => {
            let _ = runtime.shutdown().await;
            Err((format!("Cannot retrieve the MCP prompt: {error}"), true))
        }
        Err(_) => {
            let _ = runtime.shutdown().await;
            Err(("MCP prompt retrieval timed out".into(), true))
        }
    }
}

#[derive(Debug)]
enum TuiAdmissionFailure {
    Retryable(String),
    Consumed(String),
    PromptFailed { message: String, unknown: bool },
}

impl TuiAdmissionFailure {
    fn consumed(message: impl Into<String>) -> Self {
        Self::Consumed(message.into())
    }

    fn message(&self) -> &str {
        match self {
            Self::Retryable(message) | Self::Consumed(message) => message,
            Self::PromptFailed { message, .. } => message,
        }
    }
}

impl From<String> for TuiAdmissionFailure {
    fn from(message: String) -> Self {
        Self::Retryable(message)
    }
}

impl From<&str> for TuiAdmissionFailure {
    fn from(message: &str) -> Self {
        Self::Retryable(message.to_owned())
    }
}

impl TuiAdmissionTurn {
    fn run_id(&self) -> Result<&str, String> {
        let admission = self
            .manager
            .status(&self.admission_id)
            .map_err(|error| error.to_string())?;
        match &admission.state {
            polaris_core::session_manager::AdmissionState::Running { run_id, .. } => {
                Ok(run_id.as_str())
            }
            _ => Err("The sidechat run is not active".into()),
        }
    }

    fn finish(
        mut self,
        status: polaris_core::session_manager::TerminalStatus,
    ) -> Result<(), String> {
        let admission = self
            .manager
            .status(&self.admission_id)
            .map_err(|error| error.to_string())?
            .clone();
        let before = self.manager.revision;
        self.manager
            .terminal(&self.admission_id, before, admission.revision, status)
            .map_err(|error| error.to_string())?;
        self.store
            .persist(before, &self.manager)
            .map_err(|error| error.to_string())
    }
}

fn bounded_routing_text(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    let mut end = maximum;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn tui_routing_context(
    session: &polaris_core::session::Session,
) -> Result<polaris_core::session_manager::RoutingContextV1, String> {
    use polaris_core::session_manager::{CompletedUserTurnV1, RoutingContextV1};
    use polaris_provider::Role;

    let mut goal = session.workflow.as_ref().map_or_else(
        || "none".to_owned(),
        |workflow| {
            let state = workflow.state();
            if let Some(task) = &state.active_task {
                format!("active task {}", task.task_id)
            } else if let Some(contract) = &state.active_change_contract {
                format!("active change contract {}", contract.id)
            } else if let Some(feature) = &state.active_feature {
                format!("active feature {}", feature.id)
            } else {
                format!("workflow phase {}", state.phase)
            }
        },
    );
    if let Some(saved) = &session.persistence {
        let snapshot = saved.snapshot().map_err(|e| e.to_string())?;
        if let Some(saved_goal) = snapshot
            .state
            .goal
            .filter(|goal| goal.active && !goal.stopped)
        {
            goal = saved_goal.text;
        }
    }
    let users = session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User && message.tool_call_id.is_none())
        .collect::<Vec<_>>();
    let skip = users
        .len()
        .saturating_sub(polaris_core::session_manager::MAX_COMPLETED_TURNS);
    let turns = users
        .into_iter()
        .skip(skip)
        .map(|(index, message)| {
            CompletedUserTurnV1::new(
                format!("turn-{index}"),
                bounded_routing_text(
                    &message.content,
                    polaris_core::session_manager::MAX_TURN_BYTES,
                ),
            )
            .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    RoutingContextV1::new(
        bounded_routing_text(&goal, polaris_core::session_manager::MAX_GOAL_BYTES),
        turns,
    )
    .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    fs::canonicalize(path).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn open_tui_admission_owner(
    state_dir: &Path,
    current: &polaris_core::session_manager::SessionId,
) -> Result<
    (
        polaris_core::desktop_store::SessionManagerStore,
        polaris_core::session_manager::SessionManagerV1,
    ),
    String,
> {
    let root = ensure_private_directory(&state_dir.join("tui-admissions-v1"))?;
    polaris_core::desktop_store::SessionManagerStore::open_family_owned(&root, current)
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn open_tui_admission_owner(
    _state_dir: &Path,
    _current: &polaris_core::session_manager::SessionId,
) -> Result<
    (
        polaris_core::desktop_store::SessionManagerStore,
        polaris_core::session_manager::SessionManagerV1,
    ),
    String,
> {
    Err("TUI sidechat admission is unavailable on this platform".into())
}

fn persist_manager_transition(
    store: &polaris_core::desktop_store::SessionManagerStore,
    expected: u64,
    manager: &polaris_core::session_manager::SessionManagerV1,
) -> Result<(), String> {
    store
        .persist(expected, manager)
        .map_err(|error| error.to_string())
}

/// Restore accepted admissions before a TUI or one-shot CLI input is admitted.
pub fn recover_admission_owner(
    store: &polaris_core::desktop_store::SessionManagerStore,
    manager: &mut polaris_core::session_manager::SessionManagerV1,
    data_root: &Path,
    database: &Path,
    project_id: &str,
) -> Result<(), String> {
    use polaris_core::session_manager::{AdmissionState, TerminalStatus};

    let before = manager.revision;
    if manager
        .recover_classifying()
        .map_err(|error| error.to_string())?
        != 0
    {
        persist_manager_transition(store, before, manager)?;
    }
    let pending = manager
        .admissions
        .values()
        .filter(|admission| !matches!(admission.state, AdmissionState::Terminal { .. }))
        .map(|admission| admission.id.clone())
        .collect::<Vec<_>>();
    for id in pending {
        let mut admission = manager
            .status(&id)
            .map_err(|error| error.to_string())?
            .clone();
        if let AdmissionState::DestinationReady { session_id, .. } = &admission.state {
            let committed = match polaris_core::session_store::PersistedSession::open(
                data_root,
                database,
                project_id,
                session_id.as_str(),
            ) {
                Ok(saved) => {
                    let snapshot = saved.snapshot().map_err(|error| error.to_string())?;
                    match snapshot.state.admission_receipts.get(id.as_str()) {
                        Some(receipt) if receipt.payload_hash == admission.payload_hash => true,
                        Some(_) => {
                            return Err("Saved admission receipt does not match the payload".into());
                        }
                        None => false,
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.to_string()),
            };
            if committed {
                let before = manager.revision;
                admission = manager
                    .recover_input_appended(&id, before, admission.revision, session_id)
                    .map_err(|error| error.to_string())?;
                persist_manager_transition(store, before, manager)?;
            }
        }
        let before = manager.revision;
        match admission.state {
            AdmissionState::Running { .. } => {
                manager
                    .terminal(
                        &id,
                        before,
                        admission.revision,
                        TerminalStatus::OutcomeUnknown,
                    )
                    .map_err(|error| error.to_string())?;
            }
            AdmissionState::InputAppended { .. } | AdmissionState::RunQueued { .. } => {
                manager
                    .recover_unstarted_committed(&id, before, admission.revision)
                    .map_err(|error| error.to_string())?;
            }
            AdmissionState::DestinationReady { .. } => {
                manager
                    .fail_preflight(
                        &id,
                        before,
                        admission.revision,
                        TerminalStatus::OutcomeUnknown,
                    )
                    .map_err(|error| error.to_string())?;
            }
            _ => {
                manager
                    .cancel(&id, before, admission.revision)
                    .map_err(|error| error.to_string())?;
            }
        }
        persist_manager_transition(store, before, manager)?;
    }
    Ok(())
}

fn empty_sidechat_session(
    parent: &polaris_core::session::Session,
    destination: &polaris_core::session_manager::SessionId,
) -> Result<polaris_core::session::Session, String> {
    let previous = parent
        .persistence
        .as_ref()
        .ok_or("Sidechat requires a v2 session")?;
    let snapshot = previous.snapshot().map_err(|error| error.to_string())?;
    let mut next = polaris_core::session::Session::new();
    next.disable_files_md_auto_regenerate = parent.disable_files_md_auto_regenerate;
    next.input_preparation = parent.input_preparation;
    next.strict_history = parent.strict_history.clone();
    next.summary_usage = parent.summary_usage.clone();
    next.examples = parent.examples.clone();
    if let Some(workflow) = &parent.workflow {
        next.workflow = Some(
            polaris_core::workflow::SessionWorkflow::try_new(workflow.config.clone())
                .map_err(|error| error.to_string())?,
        );
    }
    let persistence = polaris_core::session_store::PersistedSession::create_with_id(
        &previous.data_root,
        &previous.database,
        &snapshot.state.project_id,
        destination.as_str(),
        snapshot.state.history_mode,
        next.workflow.as_ref(),
    )
    .map_err(|error| error.to_string())?;
    next.attach(persistence)
        .map_err(|error| error.to_string())?;
    Ok(next)
}

#[allow(clippy::too_many_arguments)]
async fn admit_tui_input(
    session: &mut polaris_core::session::Session,
    text: &str,
    decider: Option<&dyn polaris_provider::decision::Decider>,
    state_dir: &Path,
    sessions_dir: &Path,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
    turn_authority: Option<&TurnAuthorityInstaller>,
    rewind_pre_manifest_id: Option<&str>,
    tool_memory_enabled: bool,
    project: &Path,
    auto_route: bool,
    prompt_request: Option<TuiPromptRequest<'_>>,
) -> Result<
    (
        TuiAdmissionTurn,
        Option<String>,
        Option<polaris_core::conversation_state::RewindBoundaryV1>,
        Option<polaris_core::mcp::runtime::RunRuntime>,
    ),
    TuiAdmissionFailure,
> {
    use polaris_core::session_manager::{
        AdmissionState, DestinationMode, NextRunnableV1, SessionId, SubmissionPayloadV1,
        SubmitOutcomeV1, SubmitRequestV1,
    };

    require_turn_owner()?;
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("Automatic sidechat routing requires a v2 session")?;
    let snapshot = persistence.snapshot().map_err(|error| error.to_string())?;
    let current =
        SessionId::new(snapshot.state.session_id.clone()).map_err(|error| error.to_string())?;
    let session_revision = snapshot.state.generation;
    let goal_revision = snapshot
        .state
        .goal
        .as_ref()
        .map_or(0, |goal| goal.revision.get());
    let (store, mut manager) = open_tui_admission_owner(state_dir, &current)?;
    recover_admission_owner(
        &store,
        &mut manager,
        &persistence.data_root,
        &persistence.database,
        &snapshot.state.project_id,
    )?;
    if let Some(blocked) = manager.outcome_unknown_blocker(&current) {
        return Err(format!(
            "The execution state of input {} is unknown. New input cannot start in this conversation. Create another with /new or inspect saved state",
            blocked.id.as_str()
        )
        .into());
    }
    let request = SubmitRequestV1 {
        parent_session: current.clone(),
        payload: SubmissionPayloadV1 {
            text: text.to_owned(),
            attachment_ids: Vec::new(),
            route: if auto_route {
                DestinationMode::Auto
            } else {
                DestinationMode::Current
            },
            expected_session_revision: session_revision,
            expected_goal_revision: goal_revision,
        },
        admission_id: Some(
            polaris_core::session_manager::AdmissionId::new(format!("tui-{}", new_session_id()))
                .map_err(|error| error.to_string())?,
        ),
    };
    let before = manager.revision;
    manager
        .observe_session_revisions(&current, before, session_revision, goal_revision)
        .map_err(|error| error.to_string())?;
    let SubmitOutcomeV1::Accepted { admission, .. } = manager
        .submit(request, manager.revision)
        .map_err(|error| match error {
            polaris_core::session_manager::AdmissionError::Capacity(_) => {
                "This conversation has insufficient storage capacity. Continue with /fork once processing is idle"
                    .to_owned()
            }
            _ => error.to_string(),
        })?;
    let mut receipts = std::collections::BTreeMap::new();
    receipts.insert(current.clone(), snapshot.state.admission_receipts.len());
    for session_id in manager
        .sessions
        .keys()
        .filter(|id| id.as_str() != current.as_str())
    {
        let count = match polaris_core::session_store::PersistedSession::open(
            &persistence.data_root,
            &persistence.database,
            &snapshot.state.project_id,
            session_id.as_str(),
        ) {
            Ok(saved) => saved
                .snapshot()
                .map_err(|error| error.to_string())?
                .state
                .admission_receipts
                .len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.to_string().into()),
        };
        receipts.insert(session_id.clone(), count);
    }
    manager
        .check_receipt_capacity(&receipts)
        .map_err(|error| match error {
            polaris_core::session_manager::AdmissionError::Capacity(_) => {
                "This conversation has insufficient storage capacity. Continue with /fork once processing is idle"
                    .to_owned()
            }
            _ => error.to_string(),
        })?;
    store
        .persist_observed_submission(
            before,
            &manager,
            &receipts,
            &current,
            session_revision,
            goal_revision,
        )
        .map_err(|error| match error {
            polaris_core::desktop_store::StoreError::Capacity(_) => {
                "This conversation has insufficient storage capacity. Continue with /fork once processing is idle"
                    .to_owned()
            }
            _ => error.to_string(),
        })?;

    let before = manager.revision;
    let classifying = manager
        .begin_classification(&admission.id, before, admission.revision)
        .map_err(|error| error.to_string())?;
    persist_manager_transition(&store, before, &manager)?;
    let approval_reply = !session.clarification.unanswered().is_empty();
    let judgement = if !auto_route || approval_reply {
        None
    } else if let Some(decider) = decider {
        let context = tui_routing_context(session)?;
        polaris_core::session_manager::classify_context(decider, &context, text).await
    } else {
        None
    };
    let before = manager.revision;
    let decided = manager
        .decide(
            &admission.id,
            before,
            classifying.revision,
            judgement,
            approval_reply,
            false,
        )
        .map_err(|error| error.to_string())?;
    persist_manager_transition(&store, before, &manager)?;
    let before = manager.revision;
    let prepared = match manager.prepare_destination(&admission.id, before, decided.revision) {
        Ok(prepared) => prepared,
        Err(error) => {
            // A rejected new Sidechat must not leave a Decided admission at
            // the queue head. No input or host prompt has been sent yet.
            manager
                .cancel(&admission.id, before, decided.revision)
                .map_err(|cancel_error| TuiAdmissionFailure::consumed(cancel_error.to_string()))?;
            persist_manager_transition(&store, before, &manager)
                .map_err(TuiAdmissionFailure::consumed)?;
            return Err(format!("Cannot prepare the sidechat destination: {error}").into());
        }
    };
    persist_manager_transition(&store, before, &manager)?;
    let (destination, reason) = match &prepared.state {
        AdmissionState::DestinationReady { session_id, reason } => (session_id.clone(), *reason),
        _ => return Err("The sidechat destination was not finalized".into()),
    };
    let switched = destination != current;
    let mut sidechat = if switched {
        Some(empty_sidechat_session(session, &destination)?)
    } else {
        None
    };
    let destination_session = sidechat.as_mut().unwrap_or(session);
    if let Some(installer) = turn_authority
        && let Err(error) = installer(destination_session, text)
    {
        let before = manager.revision;
        let saved = manager
            .status(&admission.id)
            .map_err(|status_error| status_error.to_string())?
            .clone();
        manager
            .cancel(&admission.id, before, saved.revision)
            .map_err(|cancel_error| cancel_error.to_string())?;
        persist_manager_transition(&store, before, &manager)?;
        return Err(format!("Cannot attach routine authority: {error}").into());
    }
    let mut selected_mcp_runtime = None;
    let selected_message = if let Some(request) = prompt_request {
        let run_id = polaris_core::session_manager::RunId::derive(&admission.id);
        let selection = select_tui_prompt(request, destination.as_str(), run_id.as_str()).await;
        match selection {
            Ok((runtime, message)) => {
                selected_mcp_runtime = Some(runtime);
                Some(message)
            }
            Err((message, unknown)) => {
                let before = manager.revision;
                manager
                    .fail_preflight(
                        &admission.id,
                        before,
                        prepared.revision,
                        if unknown {
                            polaris_core::session_manager::TerminalStatus::OutcomeUnknown
                        } else {
                            polaris_core::session_manager::TerminalStatus::Failed
                        },
                    )
                    .map_err(|error| TuiAdmissionFailure::consumed(error.to_string()))?;
                persist_manager_transition(&store, before, &manager)
                    .map_err(TuiAdmissionFailure::consumed)?;
                return Err(TuiAdmissionFailure::PromptFailed { message, unknown });
            }
        }
    } else {
        None
    };
    let (rewind_boundary, rewind_warning) = match begin_rewind_for_session(
        destination_session,
        rewind_pre_manifest_id,
        project,
        state_dir,
        tool_memory_enabled,
    ) {
        Ok(boundary) => (boundary, None),
        Err(error) => (None, Some(format!("Cannot start rewind storage: {error}"))),
    };
    let appended_now = match selected_message {
        Some(message) => destination_session.push_admitted_user_with_supplement(
            text,
            message,
            &admission.id,
            &prepared.payload_hash,
        ),
        None => destination_session.push_admitted_user(text, &admission.id, &prepared.payload_hash),
    }
    .map_err(|error| TuiAdmissionFailure::consumed(error.to_string()))?;
    let before = manager.revision;
    let appended = if appended_now {
        manager
            .record_input_appended(&admission.id, before, prepared.revision, &destination)
            .map_err(|error| TuiAdmissionFailure::consumed(error.to_string()))?
    } else {
        manager
            .recover_input_appended(&admission.id, before, prepared.revision, &destination)
            .map_err(|error| TuiAdmissionFailure::consumed(error.to_string()))?
    };
    persist_manager_transition(&store, before, &manager).map_err(TuiAdmissionFailure::consumed)?;
    let before = manager.revision;
    let queued = manager
        .queue_run(&admission.id, before, appended.revision)
        .map_err(|error| TuiAdmissionFailure::consumed(error.to_string()))?;
    persist_manager_transition(&store, before, &manager).map_err(TuiAdmissionFailure::consumed)?;
    let before = manager.revision;
    let started = manager
        .start_next(before)
        .map_err(|error| TuiAdmissionFailure::consumed(error.to_string()))?;
    persist_manager_transition(&store, before, &manager).map_err(TuiAdmissionFailure::consumed)?;
    if !matches!(
        started,
        NextRunnableV1::Start {
            ref admission_id,
            ref session_id,
            ..
        } if admission_id == &admission.id && session_id == &destination
    ) {
        return Err(TuiAdmissionFailure::consumed(format!(
            "Cannot start the sidechat run: {:?}",
            queued.state
        )));
    }
    if let Some(next) = sidechat {
        *session = next;
        *session_path = sessions_dir.join(format!("{}.jsonl", destination.as_str()));
        *meta_path = session_path.with_extension("meta.json");
        *session_started_at_millis = now_millis();
        local_lines.clear();
    }
    Ok((
        TuiAdmissionTurn {
            store,
            manager,
            admission_id: admission.id,
        },
        match (switched, rewind_warning) {
            (true, Some(warning)) => Some(format!(
                "Switched to sidechat {} ({reason:?}, running) · {warning}",
                destination.as_str()
            )),
            (true, None) => Some(format!(
                "Switched to sidechat {} ({reason:?}, running)",
                destination.as_str()
            )),
            (false, warning) => warning,
        },
        rewind_boundary,
        selected_mcp_runtime,
    ))
}

struct TuiRootControlArgs<'a> {
    session_id: &'a str,
    turn: u64,
    sandbox: &'a SandboxPolicy,
    approval: ApprovalPolicy,
    mcp_available: bool,
    tool_names: &'a [String],
    model: &'a str,
    effort: &'a str,
}

fn install_tui_root_control(
    session: &mut polaris_core::session::Session,
    args: TuiRootControlArgs<'_>,
) -> Result<Arc<RootDelegationControlV1>, String> {
    let active_task = session
        .workflow
        .as_ref()
        .and_then(|workflow| workflow.state().active_task.map(|task| task.task_id))
        .unwrap_or_else(|| format!("tui-task-{}", args.turn));
    let capabilities = delegation_capabilities(
        args.sandbox,
        args.approval,
        args.mcp_available,
        args.tool_names,
    );
    let control = RootDelegationControlV1::new(RootDelegationConfigV1 {
        session_id: args.session_id.to_owned(),
        root_run_id: RootRunId::new(format!("tui-root-run-{}", args.turn))
            .map_err(|error| error.to_string())?,
        root_attempt_id: RootAttemptId::new(format!("tui-root-attempt-{}", args.turn))
            .map_err(|error| error.to_string())?,
        active_task_id: TaskId::new(active_task).map_err(|error| error.to_string())?,
        parent_capabilities: capabilities,
        model: args.model.to_owned(),
        effort: args.effort.to_owned(),
    })?;
    session.install_root_delegation_control(control.clone())?;
    Ok(control)
}

fn tui_shadow_input(
    session: &polaris_core::session::Session,
    prompt: &str,
    control: &RootDelegationControlV1,
) -> Result<Option<polaris_core::answer_finalizer::shadow::ShadowInputV1>, String> {
    use polaris_provider::Role;

    let Some(persistence) = session.persistence.as_ref() else {
        return Ok(None);
    };
    let snapshot = persistence.snapshot().map_err(|error| error.to_string())?;
    let Some(assistant) = snapshot.events.iter().rev().find(|event| {
        event.message.role == Role::Assistant
            && event.message.tool_calls.is_empty()
            && !event.message.content.is_empty()
    }) else {
        return Ok(None);
    };
    let Some(user) = snapshot.events.iter().rev().find(|event| {
        event.sequence < assistant.sequence
            && event.starts_turn
            && event.message.role == Role::User
            && event.message.content == prompt
    }) else {
        return Ok(None);
    };
    let turn_entries = snapshot
        .events
        .iter()
        .filter(|event| event.sequence > user.sequence && event.sequence < assistant.sequence);
    let mut called = std::collections::BTreeSet::new();
    let mut returned = std::collections::BTreeSet::new();
    let mut closed_evidence = Vec::new();
    let mut evidence_complete = true;
    for entry in turn_entries {
        for call in &entry.message.tool_calls {
            evidence_complete &= !call.id.is_empty() && called.insert(call.id.clone());
        }
        if entry.message.role == Role::Tool {
            let Some(id) = entry.message.tool_call_id.as_ref() else {
                evidence_complete = false;
                continue;
            };
            evidence_complete &= returned.insert(id.clone());
            closed_evidence.push(entry.message.content.clone());
        }
        for hosted in &entry.message.hosted_web_search {
            evidence_complete &= matches!(
                hosted.status,
                polaris_provider::web_search::HostedItemStatus::Completed
            );
            match serde_json::to_string(hosted) {
                Ok(value) => closed_evidence.push(value),
                Err(_) => evidence_complete = false,
            }
        }
    }
    evidence_complete &= called == returned;
    evidence_complete &= closed_evidence
        .iter()
        .try_fold(0_usize, |total, value| total.checked_add(value.len()))
        .is_some_and(|total| total <= 256 * 1024);
    let mut input_receipt_ids = snapshot
        .state
        .admission_receipts
        .iter()
        .filter(|(_, receipt)| receipt.sequence == user.sequence)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if input_receipt_ids.is_empty() {
        input_receipt_ids.push(format!("event-{}", user.sequence));
    }
    let binding = control.host_binding();
    let workflow_revision = session
        .workflow
        .as_ref()
        .map(|workflow| workflow.state().revision.to_string())
        .unwrap_or_else(|| "0".to_owned());
    let subject = polaris_core::answer_finalizer::FinalizationSubjectV1 {
        schema_version: polaris_core::answer_finalizer::FINALIZATION_SCHEMA_VERSION,
        project_id: snapshot.state.project_id,
        session_id: snapshot.state.session_id.clone(),
        session_epoch: snapshot.state.epoch.saturating_add(1),
        lane_id: snapshot.state.session_id,
        root_run_id: binding.root_run_id.0,
        attempt_id: binding.root_attempt_id.0,
        turn_sequence: assistant.sequence,
        input_receipt_ids,
        attachment_hashes: vec![],
        workflow_revision,
        exact_prompt_hash: polaris_core::conversation_state::content_hash(prompt.as_bytes()),
    };
    Ok(Some(
        polaris_core::answer_finalizer::shadow::ShadowInputV1 {
            subject,
            source_message_id: format!("event-{}", assistant.sequence),
            prompt: prompt.to_owned(),
            source_body: assistant.message.content.clone(),
            closed_evidence,
            evidence_complete,
        },
    ))
}

fn schedule_tui_shadow(
    finalizer: &Arc<polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>,
    session: &polaris_core::session::Session,
    prompt: &str,
    control: &RootDelegationControlV1,
) -> Result<bool, String> {
    let Some(input) = tui_shadow_input(session, prompt, control)? else {
        return Ok(false);
    };
    #[cfg(target_os = "linux")]
    if signal::pending_linux_process_shutdown().is_some() {
        return Ok(false);
    }
    finalizer
        .schedule(input)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn visible_rewind_memory_ids(
    session: &polaris_core::session::Session,
    project: &Path,
    state_dir: &Path,
) -> std::io::Result<Vec<String>> {
    let Some(persistence) = session.persistence.as_ref() else {
        return Ok(Vec::new());
    };
    let snapshot = persistence.snapshot()?;
    let mut origins = snapshot
        .state
        .ancestors
        .iter()
        .map(|ancestor| ancestor.session_id.clone())
        .collect::<Vec<_>>();
    origins.sort();
    origins.dedup();
    if origins.len() > MAX_TOOL_MEMORY_ORIGINS {
        return Err(std::io::Error::other("Memory lineage exceeds the limit"));
    }
    let branch = persistence.store.rewind_branch_metadata()?;
    tool_memory::visible_tool_memory_raw_object_ids(
        project,
        state_dir,
        &snapshot.state.session_id,
        &origins,
        branch
            .as_ref()
            .map(|metadata| metadata.visible_memory_raw_object_ids.as_slice()),
    )
}

fn begin_rewind_for_session(
    session: &polaris_core::session::Session,
    pre_manifest_id: Option<&str>,
    project: &Path,
    state_dir: &Path,
    tool_memory_enabled: bool,
) -> std::io::Result<Option<polaris_core::conversation_state::RewindBoundaryV1>> {
    let (Some(pre_manifest_id), Some(persistence)) =
        (pre_manifest_id, session.persistence.as_ref())
    else {
        return Ok(None);
    };
    let visible = if tool_memory_enabled {
        visible_rewind_memory_ids(session, project, state_dir)?
    } else {
        Vec::new()
    };
    persistence
        .begin_rewind_boundary(pre_manifest_id.to_owned(), visible)
        .map(Some)
}

#[cfg(unix)]
fn capture_rewind_before_input(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    policy: &SandboxPolicy,
) -> std::io::Result<polaris_core::rewind_workspace::ManifestId> {
    let lease = owner.try_mutation().map_err(std::io::Error::other)?;
    ensure_rewind_unblocked_with_lease(owner.project_root(), effects, &lease)
        .map_err(std::io::Error::other)?;
    let checkpoint = polaris_core::management_effect::ManagementEffectStoreV1::
        rewind_checkpoint_for_project_at_state_dir(owner.project_root(), effects.state_dir(), &lease)
        .map_err(std::io::Error::other)?;
    owner
        .capture_with_management_checkpoint_with_lease(&lease, policy, checkpoint)
        .map_err(std::io::Error::other)
}

#[cfg(unix)]
fn finish_rewind_for_session(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    persistence: &polaris_core::session_store::PersistedSession,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    policy: &SandboxPolicy,
    boundary: &polaris_core::conversation_state::RewindBoundaryV1,
    external_unknown: Option<String>,
    capture_can_complete: impl FnOnce() -> bool,
) -> std::io::Result<polaris_core::conversation_state::RewindBoundaryV1> {
    use polaris_core::{
        conversation_state::{RewindEffectStatusV1, RewindEligibilityV1},
        management_effect::{ManagementEffectStoreV1, ManagementRewindCrossingV1},
    };
    let captured = (|| {
        let lease = owner.try_mutation().map_err(std::io::Error::other)?;
        let checkpoint = ManagementEffectStoreV1::rewind_checkpoint_for_project_at_state_dir(
            owner.project_root(),
            effects.state_dir(),
            &lease,
        )
        .map_err(std::io::Error::other)?;
        let post = owner
            .capture_with_management_checkpoint_with_lease(&lease, policy, checkpoint)
            .map_err(std::io::Error::other)?;
        let pre = boundary
            .workspace_pre_manifest_id
            .parse()
            .map_err(std::io::Error::other)?;
        let crossing = owner
            .verify_rewind_management_with_lease(&lease, &post, &pre, true)
            .map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>((post, crossing, lease))
    })();
    match captured {
        Ok((post, crossing, _lease)) => {
            if !capture_can_complete() {
                return persistence.abandon_rewind_boundary(
                    &boundary.boundary_id,
                    "The rewind boundary is incomplete because a stop request was received".into(),
                );
            }
            let reason = match crossing {
                ManagementRewindCrossingV1::Clear => external_unknown,
                ManagementRewindCrossingV1::Blocked { .. } => {
                    Some("Includes management operations that cannot be rewound".into())
                }
            };
            let (eligibility, effect) = match reason {
                Some(description) => (
                    RewindEligibilityV1::Ineligible {
                        reason: description.clone(),
                    },
                    RewindEffectStatusV1::Unknown { description },
                ),
                None => (
                    RewindEligibilityV1::Eligible,
                    RewindEffectStatusV1::NoExternalMutation,
                ),
            };
            persistence.finish_rewind_boundary(
                &boundary.boundary_id,
                post.to_string(),
                eligibility,
                effect,
            )
        }
        Err(error) => persistence.abandon_rewind_boundary(
            &boundary.boundary_id,
            format!("Cannot verify post-turn storage evidence: {error}"),
        ),
    }
}

/// Collapses a leading `$HOME` to `~`, for the footer only (see
/// `render::HeaderInfo::cwd_short`'s docs on why the header's own
/// `directory:` row doesn't use this). Returns the path unabbreviated —
/// never an error — when `HOME` isn't set or the path doesn't start
/// under it; there's nothing meaningful to fail on here.
fn abbreviate_home(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return full;
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return full;
    }
    match full.strip_prefix(home.as_ref()) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => full,
    }
}

/// How many `scroll_offset` lines one mouse/trackpad wheel tick moves —
/// a single line per tick feels sluggish for wheel input, unlike a key
/// press (see `PageUp`/`PageDown`'s own `+1`, which is a deliberate,
/// discrete step).
const MOUSE_SCROLL_LINES: usize = 3;

/// How long a `Ctrl+O` copy confirmation stays visible mid-turn before the
/// `Thinking` animation resumes — see `mid_turn_notice_until` in `run()`.
/// Long enough for a normal glance-at-the-screen reaction, short enough
/// that it doesn't look stuck once the user has seen it.
const MID_TURN_NOTICE_DURATION: Duration = Duration::from_millis(1500);

#[derive(Debug, Default)]
struct ExitConfirmation {
    armed: bool,
}

impl ExitConfirmation {
    fn press(&mut self) -> bool {
        if self.armed {
            self.armed = false;
            return true;
        }
        self.armed = true;
        false
    }

    fn cancel(&mut self) {
        self.armed = false;
    }
}

fn is_local_provider(provider_name: &str) -> bool {
    matches!(provider_name, "ollama" | "lmstudio" | "llamacpp")
}

fn update_effort_from_event(event: &polaris_core::AgentEvent, effort_name: &mut String) {
    match event {
        polaris_core::AgentEvent::EffortJudging => *effort_name = "judging".to_string(),
        polaris_core::AgentEvent::EffortJudged { effort } => {
            *effort_name = render::effort_display_name(effort).to_string();
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InflightInputAction {
    Continue,
    Queued,
    RejectedSecret,
    RejectedUnavailable,
    Interrupt,
}

fn slash_suggestions(
    buffer: &str,
    history: &input::History,
    dispatching_queued_input: bool,
) -> Vec<&'static slash::SlashCommand> {
    // Queued and unchanged recalled commands must reach the parser intact.
    if dispatching_queued_input || history.is_recalled(buffer) {
        return Vec::new();
    }
    buffer
        .strip_prefix('/')
        .map(slash::matching)
        .unwrap_or_default()
}

#[derive(Debug, PartialEq, Eq)]
enum SlashCompletionAction {
    PassThrough,
    Navigate,
    Submit(&'static str),
}

fn slash_completion_action(
    suggestions: &[&slash::SlashCommand],
    selected: &mut usize,
    key: ratatui::crossterm::event::KeyEvent,
) -> SlashCompletionAction {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind};

    if key.kind != KeyEventKind::Press || suggestions.is_empty() {
        return SlashCompletionAction::PassThrough;
    }
    *selected %= suggestions.len();
    match key.code {
        KeyCode::Down => *selected = (*selected + 1) % suggestions.len(),
        KeyCode::Up => *selected = (*selected + suggestions.len() - 1) % suggestions.len(),
        KeyCode::Enter => return SlashCompletionAction::Submit(suggestions[*selected].name),
        _ => return SlashCompletionAction::PassThrough,
    }
    SlashCompletionAction::Navigate
}

const WINDOWS_OWNER_UNAVAILABLE: &str =
    "Windows lacks the required execution and mutation protections. No changes were made.";

fn require_turn_owner() -> Result<(), &'static str> {
    if cfg!(windows) {
        Err(WINDOWS_OWNER_UNAVAILABLE)
    } else {
        Ok(())
    }
}

fn require_action_owner(action: &slash::Action) -> Result<(), &'static str> {
    if cfg!(windows) && !action.allowed_without_mutation_owner() {
        Err(WINDOWS_OWNER_UNAVAILABLE)
    } else {
        Ok(())
    }
}

fn require_submission_owner(text: &str) -> Result<(), &'static str> {
    if text.trim().is_empty() {
        return Ok(());
    }
    match slash::parse(text) {
        Some(action) => require_action_owner(&action),
        None => require_turn_owner(),
    }
}

fn apply_admitted_key(
    buffer: &mut String,
    cursor: &mut usize,
    key: ratatui::crossterm::event::KeyEvent,
) -> Result<InputAction, &'static str> {
    #[cfg(windows)]
    {
        // Preserve the existing consume-and-scrub path for secret commands.
        if is_secret_command(buffer) {
            return Ok(apply_key(buffer, cursor, key));
        }
        let mut candidate = buffer.clone();
        let mut candidate_cursor = *cursor;
        let action = apply_key(&mut candidate, &mut candidate_cursor, key);
        if let InputAction::Submit(text) = &action {
            require_submission_owner(text)?;
        }
        *buffer = candidate;
        *cursor = candidate_cursor;
        Ok(action)
    }
    #[cfg(not(windows))]
    {
        Ok(apply_key(buffer, cursor, key))
    }
}

fn apply_composer_paste(
    buffer: &mut String,
    cursor: &mut usize,
    history: &mut input::History,
    text: &str,
) -> Result<bool, &'static str> {
    if !composer_paste_is_allowed(buffer, *cursor, text) {
        return Err("Secrets cannot be pasted into the normal input field");
    }
    let before = file_input::blocks(buffer).len();
    file_input::insert_paste(buffer, cursor, text)?;
    if redact_inline_secret_value(buffer, cursor, history) {
        return Err("Secrets cannot be pasted into the normal input field");
    }
    Ok(file_input::blocks(buffer).len() > before)
}

// Classify borrowed characters before copying a paste into ordinary input.
// The same safe MCP secret command forms as submission admission are allowed.
fn composer_paste_is_allowed(buffer: &str, cursor: usize, text: &str) -> bool {
    fn word_is<I: Iterator<Item = char>>(
        chars: &mut std::iter::Peekable<I>,
        expected: &str,
    ) -> bool {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        for expected in expected.chars() {
            if !chars
                .next()
                .is_some_and(|c| c.eq_ignore_ascii_case(&expected))
            {
                return false;
            }
        }
        chars.peek().is_none_or(|c| c.is_whitespace())
    }
    let mut chars = buffer[..cursor]
        .chars()
        .chain(text.chars())
        .chain(buffer[cursor..].chars())
        .peekable();
    if !word_is(&mut chars, "/mcp") || !word_is(&mut chars, "secret") {
        return true;
    }
    let mut set = chars.clone();
    if word_is(&mut set, "set") && set.all(char::is_whitespace) {
        return true;
    }
    if word_is(&mut chars, "delete") {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut has_reference = false;
        while chars.peek().is_some_and(|c| !c.is_whitespace()) {
            chars.next();
            has_reference = true;
        }
        return has_reference && chars.all(char::is_whitespace);
    }
    false
}

#[cfg(test)]
#[path = "tests/composer_paste.rs"]
mod composer_paste_tests;

fn promote_queued_input(
    queue: &mut VecDeque<String>,
    paused: bool,
    buffer: &mut String,
    cursor: &mut usize,
) -> Result<Option<(String, usize)>, &'static str> {
    if paused {
        return Ok(None);
    }
    let Some(next) = queue.front() else {
        return Ok(None);
    };
    require_submission_owner(next)?;
    let saved_draft = (std::mem::take(buffer), *cursor);
    *buffer = queue.pop_front().expect("checked queue front");
    *cursor = buffer.len();
    Ok(Some(saved_draft))
}

fn is_secret_command(text: &str) -> bool {
    let mut words = text.split_whitespace();
    words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("/mcp"))
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("secret"))
}

fn refuse_secret_submission(text: &mut String, local_owner_available: bool) -> bool {
    if !is_secret_command(text) {
        return false;
    }
    // Inspect borrowed words only; forbidden inline values must not be copied.
    let mut words = text.split_whitespace();
    let supported = text.starts_with('/')
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("/mcp"))
        && words.next() == Some("secret")
        && match words.next() {
            Some("set") => words.next().is_none(),
            Some("delete") => words.next().is_some() && words.next().is_none(),
            _ => false,
        };
    if local_owner_available && supported {
        return false;
    }
    text.zeroize();
    true
}

// A secret command can also be completed around text already in the composer
// (for example by moving the cursor to the start). Check the complete buffer
// before any draw, not only the next character typed after the command.
fn secret_set_has_value(input: &str) -> Option<bool> {
    let mut words = input.split_whitespace();
    if words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("/mcp"))
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("secret"))
        && words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("set"))
    {
        Some(words.next().is_some())
    } else {
        None
    }
}

fn redact_inline_secret_value(
    input: &mut String,
    cursor: &mut usize,
    history: &mut input::History,
) -> bool {
    if secret_set_has_value(input) != Some(true) {
        return false;
    }
    input.zeroize();
    history.clear_draft();
    input.push_str("/mcp secret set");
    *cursor = input.len();
    true
}

fn secret_value_key_is_blocked(input: &str, key: &ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    secret_set_has_value(input) == Some(false)
        && key.kind == KeyEventKind::Press
        && matches!(key.code, KeyCode::Char(_))
        && !(key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn apply_inflight_key(
    input_buffer: &mut String,
    input_cursor: &mut usize,
    input_history: &mut input::History,
    queued_inputs: &mut VecDeque<String>,
    key: ratatui::crossterm::event::KeyEvent,
) -> InflightInputAction {
    if secret_value_key_is_blocked(input_buffer, &key) {
        return InflightInputAction::RejectedSecret;
    }
    let action = match apply_admitted_key(input_buffer, input_cursor, key) {
        Ok(action) => action,
        Err(_) => return InflightInputAction::RejectedUnavailable,
    };
    match action {
        InputAction::Continue => {
            if redact_inline_secret_value(input_buffer, input_cursor, input_history) {
                InflightInputAction::RejectedSecret
            } else {
                InflightInputAction::Continue
            }
        }
        InputAction::Submit(text) if text.trim().is_empty() => InflightInputAction::Continue,
        InputAction::Submit(mut text) => {
            if is_secret_command(&text) {
                text.zeroize();
                return InflightInputAction::RejectedSecret;
            }
            input_history.record(&text);
            queued_inputs.push_back(text);
            InflightInputAction::Queued
        }
        InputAction::Quit => InflightInputAction::Interrupt,
    }
}

fn record_submission(
    input_history: &mut input::History,
    text: &str,
    dispatching_queued_input: bool,
) {
    if !dispatching_queued_input && !is_secret_command(text) {
        input_history.record(text);
    }
}

/// `/quit` consumes its own submission, but must leave other queued input
/// and the draft that was displaced by a queued command intact.
fn refuse_quit_with_pending_work(
    queued_inputs: &VecDeque<String>,
    input_history: &mut input::History,
    (input_buffer, input_cursor): (&mut String, &mut usize),
    saved_draft: Option<(String, usize)>,
    submitted_command: &str,
    dispatching_queued_input: bool,
    status: &mut Status,
) -> bool {
    let has_saved_draft = saved_draft
        .as_ref()
        .is_some_and(|(draft, _)| !draft.is_empty());
    if queued_inputs.is_empty()
        && input_buffer.is_empty()
        && !has_saved_draft
        && !input_history.has_hidden_draft()
    {
        return false;
    }
    if let Some((draft, cursor)) = saved_draft.filter(|(draft, _)| !draft.is_empty()) {
        *input_buffer = draft;
        *input_cursor = cursor;
    } else if input_buffer.is_empty() {
        if let Some(draft) = input_history.take_hidden_draft() {
            *input_buffer = draft;
            *input_cursor = input_buffer.len();
        } else if !dispatching_queued_input {
            input_buffer.push_str(submitted_command);
            *input_cursor = input_buffer.len();
        }
    }
    *status = Status::Notice("Handle queued input and drafts before running /quit".into());
    true
}

fn submission_resumes_queued_inputs(text: &str) -> bool {
    !matches!(
        slash::parse(text),
        Some(slash::Action::New | slash::Action::Quit)
    )
}

fn record_submission_before_dispatch(
    input_history: &mut input::History,
    text: &str,
    dispatching_queued_input: bool,
) {
    // `/new` and `/quit` can be rejected after Enter, so a saved draft
    // stays in history navigation until the command actually succeeds.
    if !matches!(
        slash::parse(text),
        Some(slash::Action::New | slash::Action::Quit)
    ) {
        record_submission(input_history, text, dispatching_queued_input);
    }
}

fn restore_rejected_new_submission(
    submitted: String,
    cursor: usize,
    queued: bool,
    input_buffer: &mut String,
    input_cursor: &mut usize,
    queued_inputs: &mut VecDeque<String>,
    queued_inputs_paused: &mut bool,
) {
    if queued {
        queued_inputs.push_front(submitted);
        *queued_inputs_paused = true;
    } else {
        *input_buffer = submitted;
        *input_cursor = cursor;
    }
}

/// Resume always rebuilds the projection after the picker returns. A successful
/// resume can reactivate the same path after replacing the in-memory session and
/// clearing local lines, so path equality is not a safe reset decision.
fn reset_conversation_view_after_resume(
    conversation_view: &mut conversation_view::ConversationViewState,
    header: &render::HeaderInfo<'_>,
    width: u16,
) {
    conversation_view.reset(header, width);
}

/// Appends every `session.messages` entry and every `local_lines` entry
/// added since the last call to `history`, once each — the in-memory
/// analogue of the old `insert_before`-based one-shot terminal print.
/// Advances `printed_messages`/`printed_local_lines` to the new lengths.
///
/// The shrink-guard below (`*printed_messages > session.messages.len()`,
/// and its `local_lines` twin) fires whenever a caller resets a session
/// out from under this function without resetting the two counters —
/// today that's only `/clear`'s `apply_slash_action` branch, which clears
/// `session.messages`/`local_lines` but has no access to `history` to
/// re-seed. Deliberately, this function does *not* try to re-seed the
/// header itself when that guard fires: doing so would need a
/// `HeaderInfo`/width threaded all the way into a function whose only
/// other job is diffing two counters against two slices, for the sake of
/// one caller. Instead every wholesale-reset path calls
/// `ConversationViewState::reset` explicitly (see every call
/// site in `run()`, including the one right after `/clear`'s
/// `apply_slash_action` call) *before* falling through to the next
/// `append_new_history` call, so by the time this function's own
/// shrink-guard would fire, it instead finds `history` already correctly
/// re-seeded and `printed_messages`/`printed_local_lines` already at 0 —
/// meaning the guard is a no-op safety net on that path, not the
/// mechanism actually doing the reset.
#[cfg(test)]
fn append_new_history(
    session: &polaris_core::session::Session,
    local_lines: &[ratatui::text::Line<'static>],
    printed_messages: &mut usize,
    printed_local_lines: &mut usize,
    history: &mut Vec<render::HistoryLine>,
) -> bool {
    conversation_view::append_new_history_parts(
        session,
        local_lines,
        printed_messages,
        printed_local_lines,
        history,
    )
}

/// Shows an input in the transcript while its admission checks run without
/// making it part of the conversation yet.  The preview is intentionally
/// removed after the one draw: persistence and `printed_messages` remain the
/// sole record of an accepted turn.
fn draw_submission_preview<E>(
    conversation_view: &mut conversation_view::ConversationViewState,
    status: &mut Status,
    text: &str,
    draw: impl FnOnce(&[render::HistoryLine], &mut render::HistoryWrapCache, &Status) -> Result<(), E>,
) -> Result<(), E> {
    let previous_status = std::mem::replace(
        status,
        Status::Notice("Preparing send (not admitted)".to_owned()),
    );
    let result = conversation_view
        .with_temporary_user_input(text, |preview, cache| draw(preview, cache, status));
    *status = previous_status;
    result
}

/// An MCP call in the failed turn may already have been sent. Keep it and
/// close unanswered calls as unknown instead of dropping the evidence; other
/// turns roll back to the user's message as before.
fn keep_or_roll_back(
    session: &mut polaris_core::session::Session,
    checkpoint: usize,
) -> std::io::Result<()> {
    if session.messages[checkpoint.min(session.messages.len())..]
        .iter()
        .flat_map(|message| &message.tool_calls)
        .any(|call| call.name.starts_with("mcp_"))
    {
        return session.recover_interrupted_tools();
    }
    session.messages.truncate(checkpoint);
    Ok(())
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A sortable-by-time conversation id: a zero-padded millisecond
/// timestamp (so filenames sort chronologically) plus the process id and
/// a per-process counter, so two ids can never collide even if generated
/// within the same millisecond.
fn new_session_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:013}-{}-{n:04}", now_millis(), std::process::id())
}

fn current_resume_id(
    session: &polaris_core::session::Session,
    session_path: &Path,
) -> Option<String> {
    if let Some(id) = session
        .persistence
        .as_ref()
        .and_then(|saved| saved.snapshot().ok())
        .map(|snapshot| snapshot.state.session_id)
    {
        return Some(id);
    }

    let meta_path = session_path.with_extension("meta.json");
    if !session_path.is_file() && !meta_path.is_file() {
        return None;
    }
    session_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn resume_hint(session_id: &str) -> String {
    format!("\nResume this session with:\npolaris --resume {session_id}\n")
}

fn print_resume_hint(session_id: &str) {
    use ratatui::crossterm::style::{Color, Print, ResetColor, SetForegroundColor};

    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        SetForegroundColor(Color::DarkGrey),
        Print(resume_hint(session_id)),
        ResetColor
    );
}

/// A saved conversation may be resumed only against the workspace version
/// recorded at its current raw head. In particular, rewinding one branch
/// must not silently run an older conversation against the restored files.
#[cfg(unix)]
fn validate_resume_workspace(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    persistence: &polaris_core::session_store::PersistedSession,
) -> Result<(), String> {
    polaris_core::rewind_admission::validate_execution(owner, persistence)
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn open_rewind_workspace(
    cwd: &std::path::Path,
    state_dir: &std::path::Path,
) -> Result<polaris_core::rewind_workspace::WorkspaceOwner, String> {
    use polaris_core::rewind_workspace::{ResolveOutcome, RewindLimits, WorkspaceOwner};
    let project_root = polaris_core::project::resolve_root(cwd);
    let owner = WorkspaceOwner::open(
        &project_root,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .map_err(|error| format!("Cannot verify rewind storage: {error}"))?;
    match owner.resolve_pending() {
        Ok(ResolveOutcome::Clean) => Ok(owner),
        Ok(ResolveOutcome::RolledBack(_)) => Err(
            "Recovered files from an interrupted rewind. Restart Polaris to reload settings."
                .into(),
        ),
        Err(error) => Err(format!(
            "Cannot verify recovery from an interrupted rewind: {error}"
        )),
    }
}

#[cfg(unix)]
fn show_persisted_read_only_resume<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    persistence: &polaris_core::session_store::PersistedSession,
    reason: &str,
) -> Result<(), String> {
    let snapshot = persistence.snapshot().map_err(|error| error.to_string())?;
    let messages = snapshot
        .events
        .into_iter()
        .map(|event| event.message)
        .collect::<Vec<_>>();
    let source = format!("Saved conversation (view only): {reason}");
    show_read_only_transcript(terminal, reader, &messages, &source);
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn show_startup_read_only_resume(
    persistence: &polaris_core::session_store::PersistedSession,
    reason: &str,
) -> Result<(), String> {
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    )
    .map_err(|error| error.to_string())?;
    let terminal = RefCell::new(crate::terminal::init_with_options(
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fullscreen,
        },
    ));
    let mut reader = CrosstermKeyReader;
    let result = show_persisted_read_only_resume(&terminal, &mut reader, persistence, reason);
    crate::terminal::restore();
    result
}

#[cfg(all(unix, not(target_os = "linux")))]
fn show_startup_workflow_confirmation(view: &render::WorkflowChangeConfirmView<'_>) -> bool {
    if ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    )
    .is_err()
    {
        return false;
    }
    let terminal = RefCell::new(crate::terminal::init_with_options(
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fullscreen,
        },
    ));
    let mut reader = CrosstermKeyReader;
    let accepted = confirm_workflow_change(&terminal, &mut reader, view);
    crate::terminal::restore();
    accepted
}

#[cfg(target_os = "linux")]
fn show_startup_read_only_resume(
    persistence: &polaris_core::session_store::PersistedSession,
    reason: &str,
) -> Result<(), String> {
    let (mut guard, terminal) = match linux_start_terminal(
        signal::pending_linux_process_shutdown,
        LinuxTerminalGuard::enter,
    ) {
        Ok(started) => started,
        // This viewer is only entered after an existing startup failure.
        Err(LinuxTerminalStartError::Shutdown(_)) => return Ok(()),
        Err(LinuxTerminalStartError::Failed(error)) => return Err(error),
    };
    let terminal = RefCell::new(terminal);
    let mut reader = LinuxSignalAwareKeyReader;
    let result = show_persisted_read_only_resume(&terminal, &mut reader, persistence, reason);
    linux_finish_guarded_terminal(result, &mut guard, linux_restore_terminal_step)
}

#[cfg(target_os = "linux")]
fn show_startup_workflow_confirmation_checked(
    view: &render::WorkflowChangeConfirmView<'_>,
) -> Result<bool, String> {
    let (mut guard, terminal) = match linux_start_terminal(
        signal::pending_linux_process_shutdown,
        LinuxTerminalGuard::enter,
    ) {
        Ok(started) => started,
        Err(LinuxTerminalStartError::Shutdown(_)) => return Ok(false),
        Err(LinuxTerminalStartError::Failed(error)) => return Err(error),
    };
    let terminal = RefCell::new(terminal);
    let mut reader = LinuxSignalAwareKeyReader;
    let accepted = confirm_workflow_change(&terminal, &mut reader, view)
        && signal::pending_linux_process_shutdown().is_none();
    linux_finish_guarded_terminal(Ok(accepted), &mut guard, linux_restore_terminal_step)
}

/// Splits `frame.area()` into the scrollable history region (top) and the
/// footer region (bottom, `render::footer_height(suggestions.len())` rows —
/// sized fresh each frame for exactly what this frame's footer content
/// needs, not a fixed worst-case reservation, so `history_area` naturally
/// grows to fill whatever the footer isn't using and the footer sits flush
/// against the bottom with no leftover gap above it), renders the current
/// `visible_history_window` slice of `history` into the first, and calls
/// `render_footer` with the second. This is the one draw routine every
/// redraw point in `run()` shares — see this plan's Task 6.
#[allow(clippy::too_many_arguments)]
fn draw_frame(
    frame: &mut ratatui::Frame,
    history: &[render::HistoryLine],
    history_wrap_cache: &mut render::HistoryWrapCache,
    scroll_offset: usize,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &render::HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
    selection: &Option<selection::Selection>,
) {
    draw_frame_with_mode(
        frame,
        history,
        history_wrap_cache,
        scroll_offset,
        input,
        cursor,
        status,
        header,
        suggestions,
        selected_suggestion,
        selection,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_frame_with_mode(
    frame: &mut ratatui::Frame,
    history: &[render::HistoryLine],
    history_wrap_cache: &mut render::HistoryWrapCache,
    scroll_offset: usize,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &render::HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
    selection: &Option<selection::Selection>,
    read_only: bool,
) {
    let area = frame.area();
    let footer_height = render::footer_height(suggestions.len());
    let [history_area, footer_area] = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(0),
        ratatui::layout::Constraint::Length(footer_height),
    ])
    .areas(area);

    // Width/source changes invalidate the cached reflow. Scroll, input and
    // status-only redraws reuse it, so navigation cost follows the visible
    // window rather than the full conversation.
    let wrapped = history_wrap_cache.lines(history, history_area.width);
    let window =
        render::visible_history_window(wrapped.len(), scroll_offset, history_area.height as usize);
    render::render_history_into(frame.buffer_mut(), history_area, &wrapped[window.clone()]);
    if let Some(sel) = selection {
        render::apply_selection_highlight(frame.buffer_mut(), history_area, window, wrapped, sel);
    }

    render::render_footer_with_mode(
        frame,
        footer_area,
        input,
        cursor,
        status,
        header,
        suggestions,
        selected_suggestion,
        read_only,
    );
}

/// Draws a normal chat frame, then replays the input row in full.
///
/// IME composition can clear the terminal's unused cells after ratatui has
/// already recorded the row as unchanged. Replaying the completed row from an
/// empty buffer repairs that external damage without clearing history or
/// synthesizing cells for wide characters.
fn draw_chat_frame<B: Backend>(
    terminal: &mut ratatui::Terminal<B>,
    input: &str,
    cursor: usize,
    suggestion_count: usize,
    draw: impl FnOnce(&mut ratatui::Frame),
) -> io::Result<()> {
    let completed = terminal.draw(draw)?;
    let Some((input_area, cursor_position)) =
        render::composer_geometry(completed.area, suggestion_count, input, cursor)
    else {
        return Ok(());
    };

    let mut row = ratatui::buffer::Buffer::empty(input_area);
    for y in input_area.top()..input_area.bottom() {
        for x in input_area.left()..input_area.right() {
            row[(x, y)] = completed.buffer[(x, y)].clone();
        }
    }
    let empty = ratatui::buffer::Buffer::empty(input_area);
    terminal.backend_mut().draw(empty.diff(&row).into_iter())?;
    terminal.set_cursor_position(cursor_position)?;
    terminal.backend_mut().flush()
}

/// The terminal's current (width, height), or a fallback if the size
/// can't be read (matches the fallback other `run()` call sites already
/// use — see e.g. `abbreviate_home`'s caller).
fn terminal_size(terminal: &RefCell<crate::terminal::DefaultTerminal>) -> (u16, u16) {
    terminal
        .borrow()
        .size()
        .map(|s| (s.width, s.height))
        .unwrap_or((80, 24))
}

fn terminal_width(terminal: &RefCell<crate::terminal::DefaultTerminal>) -> u16 {
    terminal_size(terminal).0
}

fn terminal_height(terminal: &RefCell<crate::terminal::DefaultTerminal>) -> u16 {
    terminal_size(terminal).1
}

/// The history area's height, given the terminal's total height — mirrors
/// `draw_frame`'s own `Layout::vertical([Constraint::Min(0),
/// Constraint::Length(footer_height)])` split without needing a `Frame`
/// to do it (mouse-event handling runs outside the `draw` closure). Callers
/// pass `render::footer_height(...)` for `footer_height`, computed from
/// whatever suggestion count this call site's `draw_frame` calls actually
/// use — see the four call sites below.
///
/// Duplicates a layout `draw_frame` already computes inline — a future
/// refactor could have `draw_frame` itself call these helpers, but that's
/// out of scope for this plan.
fn area_height_for_history(footer_height: u16, terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(footer_height)
}

pub async fn run(mut args: RunArgs<'_>) -> ExitCode {
    if let Err(error) = require_turn_owner() {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    let usage_meter = polaris_provider::UsageMeter::default();
    args.provider = Arc::new(usage_meter.wrap(args.provider.clone()));
    let cloud_provider = args.provider.clone();
    let cloud_provider_name = args.provider_name.clone();
    let mut provider_name = cloud_provider_name.clone();
    let local_router = polaris_core::local_execution::LocalRouter::new(args.spawn_concurrency);
    args.mcp_selection_provider = args
        .mcp_selection_provider
        .map(|provider| Arc::new(usage_meter.wrap(provider)) as Arc<dyn Provider>);
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        eprintln!("polaris: refusing to start the TUI on a non-interactive terminal");
        return ExitCode::FAILURE;
    }
    #[cfg(target_os = "linux")]
    if let Some(code) = linux_pending_exit_code() {
        return code;
    }

    #[cfg(target_os = "linux")]
    if let Err(error) = source_apply::prepare_state(&args.cwd, &args.state_dir) {
        eprintln!("Cannot prepare storage: {error}");
        return ExitCode::FAILURE;
    }

    // These catalogs are process-owned. A successful rewind rebuilds them
    // from the restored project tree before its branch becomes active.
    let mut effective_skills = args.skills.to_vec();
    let mut effective_agent_types = args.agent_types.to_vec();

    #[cfg(unix)]
    let rewind_workspace = {
        match open_rewind_workspace(&args.cwd, &args.state_dir) {
            Ok(owner) => Some(owner),
            Err(error) => {
                if let Some(continuation) = &args.continuation
                    && let Err(viewer_error) =
                        show_startup_read_only_resume(&continuation.persistence, &error)
                {
                    eprintln!("Cannot view the saved conversation: {viewer_error}");
                }
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
    };

    // Recover the management journal only after pending workspace file
    // transactions have been resolved. Opening needs the same exclusive
    // project lease as rewind; ordinary turns take its shared side below.
    #[cfg(unix)]
    let (management_effects, workflow_recovery_issue, management_startup_blocker) = {
        let owner = rewind_workspace
            .as_ref()
            .expect("Unix TUI startup requires a rewind workspace owner");
        let lease = match owner.try_mutation() {
            Ok(lease) => lease,
            Err(error) => {
                eprintln!("Cannot start management operation recovery: {error}");
                return ExitCode::FAILURE;
            }
        };
        let project_root = polaris_core::project::resolve_root(&args.cwd);
        let store =
            match polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
                &project_root,
                &args.state_dir,
                &lease,
            ) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("Cannot recover management operation records: {error}");
                    return ExitCode::FAILURE;
                }
            };
        let Some(data_root) = args.sessions_dir.parent() else {
            eprintln!("Cannot locate v2 session storage");
            return ExitCode::FAILURE;
        };
        let project_id = match persist::project_identity(&project_root) {
            Ok(project_id) => project_id,
            Err(error) => {
                eprintln!("Cannot retrieve the project identifier: {error}");
                return ExitCode::FAILURE;
            }
        };
        let switch = polaris_core::session_switch::SessionSwitchOwner {
            effects: &store,
            lease: &lease,
            state_dir: &args.state_dir,
        };
        if let Err(error) = switch.recover(
            data_root,
            &args.state_dir.join("memory.sqlite3"),
            &project_id,
        ) {
            eprintln!("Cannot recover conversation switch evidence: {error}");
            return ExitCode::FAILURE;
        }
        let workflow_owner =
            polaris_core::management_effect::workflow_change::WorkflowChangeOwner {
                effects: &store,
                lease: &lease,
                state_dir: &args.state_dir,
            };
        let recovery_issue = workflow_owner
            .recover(
                data_root,
                &args.state_dir.join("memory.sqlite3"),
                &project_id,
            )
            .err()
            .map(|error| error.to_string());
        let blocker =
            match polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_mutation(
                &project_root,
                &args.state_dir,
                &lease,
            ) {
                Ok(value) => value.map(|effect| {
                    format!(
                        "Unresolved management operation {} ({}). New input cannot start{}",
                        effect.request.operation_id,
                        effect.request.inventory,
                        recovery_issue
                            .as_ref()
                            .map(|reason| format!(". Recovery reason: {reason}"))
                            .unwrap_or_default()
                    )
                }),
                Err(error) => Some(format!("Cannot verify management and sync state: {error}")),
            };
        (store, recovery_issue, blocker)
    };

    #[cfg(unix)]
    if let Some(continuation) = &args.continuation {
        if management_startup_blocker.is_none()
            && let Err(error) = continuation
                .persistence
                .abandon_unfinished_rewind_boundaries(
                    "The previous execution exited before completion".into(),
                )
        {
            eprintln!("Cannot verify the incomplete rewind boundary: {error}");
            return ExitCode::FAILURE;
        }
        if let Some(owner) = &rewind_workspace
            && let Err(error) = validate_resume_workspace(owner, &continuation.persistence)
        {
            if let Err(viewer_error) =
                show_startup_read_only_resume(&continuation.persistence, &error)
            {
                eprintln!("Cannot view the saved conversation: {viewer_error}");
            }
            eprintln!("Cannot verify workspace files for the saved conversation: {error}");
            return ExitCode::FAILURE;
        }
    }

    // A launch without an explicit continuation starts empty: there is no
    // implicit session per project. Its JSONL id remains unwritten until the
    // first message; a prepared continuation retains its resolved v2 id.
    let blocked_session = {
        #[cfg(unix)]
        {
            management_startup_blocker.as_ref().map(|_| {
                let mut session = polaris_core::session::Session::new();
                session.input_preparation = args.input_preparation;
                session.disable_files_md_auto_regenerate = !args.files_md_auto_regenerate;
                session.approval_auto = args.approval_auto;
                session
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(code) = linux_pending_exit_code() {
        return code;
    }
    let mut session = match if let Some(blocked) = blocked_session {
        Ok(blocked)
    } else {
        new_workflow_session(WorkflowSessionArgs {
            config: &args.workflow_config,
            initial_phase: args.initial_phase,
            history_mode: args.history_mode,
            input_preparation: args.input_preparation,
            cwd: &args.cwd,
            state_dir: &args.state_dir,
            sessions_dir: &args.sessions_dir,
            strict_history: args.strict_history.clone(),
            summary_usage: args.summary_usage.clone(),
            history_backend_factory: args.history_backend_factory.as_ref(),
            files_md_auto_regenerate: args.files_md_auto_regenerate,
            approval_auto: args.approval_auto,
            continuation: args.continuation.clone(),
            workflow_mode_overridden: args.workflow_mode_overridden,
            skills: &effective_skills,
            #[cfg(unix)]
            management_effects: Some(&management_effects),
            #[cfg(unix)]
            project_mutation: rewind_workspace.as_ref(),
        })
        .await
    } {
        Ok(session) => session,
        Err(error) => {
            #[cfg(target_os = "linux")]
            if error == LINUX_SHUTDOWN_DURING_RESTORE {
                if let Some(code) = linux_pending_exit_code() {
                    return code;
                }
            }
            eprintln!("Can't start the workflow session: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(code) = linux_pending_exit_code() {
        return code;
    }
    if args
        .continuation
        .as_ref()
        .is_some_and(|continuation| continuation.kind == ContinuationKind::Resume)
        && let Err(error) = session.recover_history().await
    {
        eprintln!("Cannot restore strict10 history: {error}");
        return ExitCode::FAILURE;
    }
    #[cfg(target_os = "linux")]
    if let Some(code) = linux_pending_exit_code() {
        return code;
    }
    #[cfg(unix)]
    if management_startup_blocker.is_none()
        && args.continuation.is_some()
        && args.workflow_mode_overridden
    {
        let candidate =
            match workflow_override_candidate(&session, &args.workflow_config, &effective_skills) {
                Ok(candidate) => candidate,
                Err(error) => {
                    eprintln!("Cannot verify the workflow change: {error}");
                    return ExitCode::FAILURE;
                }
            };
        let workspace = rewind_workspace
            .as_ref()
            .expect("Unix TUI startup requires a rewind workspace owner");
        #[cfg(target_os = "linux")]
        let startup_terminal_error = RefCell::new(None);
        #[cfg(target_os = "linux")]
        let confirm = |view: &render::WorkflowChangeConfirmView<'_>| {
            match show_startup_workflow_confirmation_checked(view) {
                Ok(accepted) => accepted,
                Err(error) => {
                    *startup_terminal_error.borrow_mut() = Some(error);
                    false
                }
            }
        };
        #[cfg(not(target_os = "linux"))]
        let confirm = show_startup_workflow_confirmation;
        let workflow_change = apply_confirmed_workflow_change(
            &mut session,
            candidate,
            polaris_core::management_effect::workflow_change::WorkflowEffectInventoryV1::F15,
            workspace,
            &management_effects,
            &args.state_dir,
            confirm,
        );
        #[cfg(target_os = "linux")]
        if let Some(error) = startup_terminal_error.into_inner() {
            eprintln!("Cannot recover the workflow confirmation screen: {error}");
            return ExitCode::FAILURE;
        }
        match workflow_change {
            Ok(WorkflowChangeResult::Changed | WorkflowChangeResult::NoChange) => {}
            Ok(WorkflowChangeResult::Cancelled) => {
                #[cfg(target_os = "linux")]
                if let Some(code) = linux_pending_exit_code() {
                    return code;
                }
                eprintln!("Workflow change cancelled");
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("Cannot save the workflow change: {error}");
                return ExitCode::FAILURE;
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(code) = linux_pending_exit_code() {
            return code;
        }
    }
    let mut session_started_at_millis = now_millis();
    let session_id = session
        .persistence
        .as_ref()
        .and_then(|saved| saved.snapshot().ok())
        .map(|snapshot| snapshot.state.session_id)
        .unwrap_or_else(new_session_id);
    let mut session_path = args.sessions_dir.join(format!("{session_id}.jsonl"));
    let mut meta_path = session_path.with_extension("meta.json");

    // One handle for the whole process, shared with any subagent `spawn`
    // starts — see `agent::run`'s docs on why the audit log is shared
    // rather than borrowed exclusively.
    let audit = match AuditLog::open(&args.audit_path) {
        Ok(a) => Arc::new(tokio::sync::Mutex::new(a)),
        Err(e) => {
            eprintln!("Can't open the audit log: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(meter) = args.decider.as_ref().and_then(|held| held.telemetry()) {
        polaris_core::audit::install_decision_observer(&meter, audit.clone());
    }
    args.mcp_selection_provider = args
        .mcp_selection_provider
        .map(|provider| polaris_core::audit::observe_arbitration(provider, audit.clone()));

    #[cfg(target_os = "linux")]
    let mut source_frontend = source_apply::Frontend::open(&args, audit.clone());

    // Shared via `RefCell`, not held exclusively — the agent-turn loop
    // below redraws the screen concurrently with `agent::run` (to animate
    // the status row while it's in flight), including while `agent::run`
    // is synchronously inside an approval prompt that also draws to this
    // same terminal (see `approver::TuiApprover`'s docs).
    //
    // A self-managed `Viewport::Fullscreen`, not native terminal
    // scrollback — every terminal except a rare few force-scrolls to the
    // bottom on new output or a keystroke (verified against Terminal.app,
    // which offers no way to disable it), which broke the earlier inline-
    // viewport design's premise of a real, user-scrollable primary-buffer
    // history. The header and every conversation turn are appended once to
    // the in-memory `history` buffer (see `append_new_history` below); each
    // frame, `draw_frame` renders `render::visible_history_window`'s slice
    // of that buffer (governed by `scroll_offset` below) into the top
    // region and `render::render_footer` (suggestions/status/input/footer)
    // into the bottom region, sized fresh each frame by
    // `render::footer_height` — the one draw routine every redraw point in
    // this loop shares.
    // The alternate screen keeps the shell scrollback intact. Linux owns
    // each acquired terminal state through a guard; other platforms retain
    // the existing ratatui initialization and restoration path.
    #[cfg(not(target_os = "linux"))]
    if let Err(e) = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    ) {
        eprintln!("Can't enter the alternate screen: {e}");
        return ExitCode::FAILURE;
    }
    #[cfg(not(target_os = "linux"))]
    let terminal = RefCell::new(crate::terminal::init_with_options(
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fullscreen,
        },
    ));
    #[cfg(target_os = "linux")]
    let (mut terminal_guard, terminal) = match linux_start_terminal(
        signal::pending_linux_process_shutdown,
        LinuxTerminalGuard::enter,
    ) {
        Ok(started) => started,
        Err(LinuxTerminalStartError::Shutdown(reason)) => return linux_signal_exit_code(reason),
        Err(LinuxTerminalStartError::Failed(error)) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(target_os = "linux")]
    let terminal = {
        terminal_guard.enable_mouse_and_cursor();
        RefCell::new(terminal)
    };
    // Trackpad/mouse wheel scrolling and shared visible-cell drag selection
    // (see `terminal.rs`) — without capture, crossterm never
    // emits `Event::Mouse` at all. This blocks the terminal's own native
    // mouse handling (including drag-select) while polaris is running — see
    // `selection.rs`'s module doc for why polaris implements its own
    // selection instead of relying on that. The old history-only selection is
    // retained for non-wrapped backends. Best-effort: a terminal that
    // doesn't support mouse reporting just keeps not sending mouse events,
    // same as before.
    #[cfg(not(target_os = "linux"))]
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture,
        ratatui::crossterm::event::EnableBracketedPaste
    );
    // A thin bar cursor, not the terminal's default (usually a full
    // blinking block) — a block cursor fully inverts whatever glyph sits
    // in that cell, so parking it over the empty input box's placeholder
    // text ("Ask polaris to do anything") made the caret look like a
    // blinking capital "A" rather than a caret. A bar sits between
    // characters instead of on top of one, so this can't happen
    // regardless of what's under it. Best-effort: an unsupported terminal
    // just keeps its own default cursor shape, which is cosmetic only.
    #[cfg(not(target_os = "linux"))]
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::SetCursorStyle::BlinkingBar
    );
    let startup_width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
    let mut conversation_view = conversation_view::ConversationViewState::new(
        &render::HeaderInfo {
            provider_name: &provider_name,
            model_name: &args.model_name,
            usage: polaris_provider::Usage::default(),
            cwd: &args.cwd.display().to_string(),
            cwd_short: "",
            effort_name: "",
        },
        startup_width,
    );

    let cwd_display = args.cwd.display().to_string();
    // Shown only in the footer — verified against a real codex
    // screenshot (`gpt-5.6-sol high · ~/File/projects/...`); the header
    // box's own `directory:` row keeps the unabbreviated `cwd_display`.
    let cwd_footer_display = abbreviate_home(&args.cwd);
    let mut input_buffer = String::new();
    // Byte offset into `input_buffer`, always on a UTF-8 char boundary —
    // see `input::apply_key`.
    let mut input_cursor: usize = 0;
    // Session-local Up/Down recall of previously submitted input. Not
    // persisted — see `input::History`.
    let mut input_history = input::History::new();
    // Submitted while another turn is running. These are dispatched in order
    // through the ordinary input path after that turn finishes; the in-flight
    // turn is never mutated concurrently.
    let mut queued_inputs = VecDeque::new();
    #[cfg(unix)]
    let mut queued_instructions = VecDeque::<instruction_commands::QueuedInstructionV1>::new();
    #[cfg(unix)]
    let mut active_instruction: Option<instruction_commands::QueuedInstructionV1> = None;
    // A local copy, not `args.approval_policy` directly — `/permissions`
    // needs to be able to change it for the rest of the session, and
    // `args` isn't mutable (nor should adding one slash command make it
    // become mutable everywhere else it's used).
    let mut approval_policy = args.approval_policy;
    // A local copy, not `args.model_name` directly — `/model` needs to be
    // able to change it for the rest of the session, same reasoning as
    // `approval_policy` above. `args.provider.set_model(...)` is the one
    // that actually changes what gets sent; this is only the display copy
    // shown in the header/footer/notices.
    let mut model_name = args.model_name.clone();
    // An explicit caller selection remains fixed. Otherwise hosted turns
    // begin undecided and Jev publishes the selected effort during the turn.
    // The caller passes a wire token (`--effort`'s own spelling); from here
    // on everything is display names, so `xhigh` becomes the row it belongs
    // to instead of a label no catalog row matches.
    let cloud_effort_name = args
        .initial_effort_name
        .as_deref()
        .map(render::effort_display_name)
        .unwrap_or(UNDECIDED_EFFORT)
        .to_string();
    let mut effort_name = cloud_effort_name.clone();
    let mut status = {
        #[cfg(unix)]
        {
            management_startup_blocker
                .map(Status::Error)
                .unwrap_or(Status::Idle)
        }
        #[cfg(not(unix))]
        {
            Status::Idle
        }
    };
    #[cfg(unix)]
    let mut rewind_unavailable_reason = "No saved rewind boundaries yet".to_owned();
    let mut exit_confirmation = ExitConfirmation::default();
    let mut rewind_gesture = rewind_ui::DoubleEscape::default();
    #[cfg(not(target_os = "linux"))]
    let mut key_reader = CrosstermKeyReader;
    #[cfg(target_os = "linux")]
    let mut key_reader = LinuxSignalAwareKeyReader;
    // Each turn gets its own MCP runtime; connections and grants never carry over.
    let mcp_ceiling =
        polaris_core::mcp::operation::ExternalCeiling::host_external(args.mcp_external);
    let mut mcp_host = polaris_core::mcp::local::LocalHost::open(&args.cwd, false).ok();
    let mut mcp_setup = mcp_host
        .as_ref()
        .map(|host| Arc::new(host.setup_tool(mcp_ceiling)));
    let setting = Arc::new(polaris_core::recipe::tool::SettingTool::new(
        polaris_core::project::resolve_root(&args.cwd),
    ));
    let computer = polaris_core::computer::ComputerTool::available().map(Arc::new);
    let mut mcp_turns = 0u64;
    let mut pending_mcp_prompt: Option<polaris_core::mcp::runtime::McpPromptChoice> = None;
    let mut mcp_prompt_requires_reselect = false;
    let mut queued_inputs_paused = false;
    let mut fatal_message: Option<String> = None;
    // Set when a `HistoryCompacted` event is drained mid-turn (see the
    // `events_rx.recv()`/`try_recv` sites below) — `session.messages`
    // can't be touched at that point (`agent_future` still holds it
    // mutably borrowed), so this just remembers that a resync is owed.
    // Declared here, outside `'outer`, rather than freshly per turn:
    // `TurnOutcome::Done(Ok(_))` is the only place that clears it back to
    // `false` (after actually persisting), so if compaction fires during
    // a turn that then errors or is interrupted, the flag stays `true`
    // and the *next* successful turn still catches the resync up in one
    // `persist::rewrite` — see that match arm below.
    let mut history_compacted_this_turn = false;
    // Which row the `/`-popup highlights. Persists across loop iterations
    // (arrow keys move it) and is clamped below whenever the candidate
    // list itself changes, so it's always a valid index or the list is
    // empty.
    let mut selected_suggestion: usize = 0;
    // Local-only output appended after the real conversation — e.g.
    // `/diff`'s result. Never touches `session.messages`, never sent to
    // the model, never persisted; cleared by `/clear`/`/new` along with
    // the real history.
    let mut local_lines: Vec<ratatui::text::Line<'static>> = Vec::new();

    #[cfg(unix)]
    let mut initial_saved_settings =
        match polaris_core::session_settings::SavedSessionSettingsV1::new(
            provider_name.clone(),
            args.model_name.clone(),
            args.initial_effort_name
                .clone()
                .unwrap_or_else(|| "auto".into()),
        ) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("Cannot verify initial settings: {error}");
                return ExitCode::FAILURE;
            }
        };
    #[cfg(unix)]
    {
        use polaris_core::{session_settings::ToolMemoryModeV1, tool_memory::RetentionMode};
        initial_saved_settings.remember = args.remember;
        initial_saved_settings.tool_memory = match args.tool_memory {
            Some(RetentionMode::History) => ToolMemoryModeV1::History,
            Some(RetentionMode::Retrieval) => ToolMemoryModeV1::Retrieval,
            None => ToolMemoryModeV1::Off,
        };
        initial_saved_settings.compact_threshold = args.compact_at;
        initial_saved_settings.tool_memory_embedding = match args
            .tool_memory_embedding
            .clone()
            .map(|(url, model)| {
                polaris_core::session_settings::SavedToolEmbeddingV1::new(url, model)
            })
            .transpose()
        {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("Cannot verify initial embedding settings: {error}");
                return ExitCode::FAILURE;
            }
        };
    }
    #[cfg(unix)]
    let mut effective_saved_settings = initial_saved_settings.clone();
    #[cfg(unix)]
    let mut saved_settings_path: Option<PathBuf> = None;
    #[cfg(unix)]
    let mut refresh_saved_settings = true;
    #[cfg(unix)]
    let mut draft_candidate = String::new();
    #[cfg(unix)]
    let mut draft_saved_text = String::new();
    #[cfg(target_os = "linux")]
    let mut draft_changed_at = Instant::now();
    let mut next_max_turns = args.max_turns;
    #[cfg(unix)]
    let mut last_submission_was_local = false;
    #[cfg(target_os = "linux")]
    let workspace_commands = {
        use std::os::unix::fs::MetadataExt;
        let root = rewind_workspace
            .as_ref()
            .expect("startup owner")
            .project_root()
            .to_owned();
        std::fs::symlink_metadata(&root)
            .map_err(|e| e.to_string())
            .and_then(|meta| {
                workspace_commands::WorkspaceCommands::new(
                    root,
                    polaris_desktop_service::WorkspaceSourceIdentity {
                        device: meta.dev(),
                        inode: meta.ino(),
                    },
                )
            })
    };
    let mut tool_memory_path: Option<PathBuf> = None;
    #[cfg(target_os = "macos")]
    let mut pending_c10_job: Option<C10TuiJob> = None;
    #[cfg(target_os = "macos")]
    let mut c10_exit_requested = false;
    let exit_code = 'outer: loop {
        #[cfg(target_os = "linux")]
        if let Some(code) = linux_pending_exit_code() {
            break code;
        }
        #[cfg(target_os = "macos")]
        if let Some(job) = pending_c10_job.as_mut() {
            let result = job.poll(
                |view| {
                    terminal
                        .borrow_mut()
                        .draw(|frame| render::render_c10_preview(frame, view))
                        .is_ok()
                },
                |view| confirm_c10_sync(&terminal, &mut key_reader, view),
            );
            if let Some(result) = result {
                pending_c10_job.take();
                let failed = result.is_err();
                record_c10_result(result, &mut status, &mut local_lines);
                if c10_exit_requested {
                    break if failed {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::SUCCESS
                    };
                }
                continue;
            }
            if terminal.borrow_mut().draw(render::render_c10_wait).is_err() {
                // Dropping the owned job revokes UI, closes prompts and reaps.
                break ExitCode::FAILURE;
            }
            match ratatui::crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crate::terminal::read_event() {
                    Ok(ratatui::crossterm::event::Event::Key(key))
                        if key.code == ratatui::crossterm::event::KeyCode::Char('c')
                            && key
                                .modifiers
                                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        c10_exit_requested = true;
                    }
                    Ok(_) => {}
                    Err(_) => break ExitCode::FAILURE,
                },
                Ok(false) => {}
                Err(_) => break ExitCode::FAILURE,
            }
            // This is the main event loop: redraw/resize/exit events continue
            // while a network worker runs; no input enters history or a queue.
            continue;
        }
        if redact_inline_secret_value(&mut input_buffer, &mut input_cursor, &mut input_history) {
            status = Status::Error("Secrets cannot be entered in the normal input field".into());
        }
        // `/new`, `/fork`, and `/resume` replace the conversation owner.
        // Reattach process-owned services and selectors before accepting the
        // next input; none of these belong to durable conversation state.
        #[cfg(unix)]
        {
            // `/model` changes the process binding without saving preferences.
            // Unconfigured conversations and first-time settings writes must
            // use that current binding; durable settings still take precedence.
            initial_saved_settings.provider.clone_from(&provider_name);
            initial_saved_settings.model.clone_from(&model_name);
            initial_saved_settings.effort = if is_local_provider(&provider_name) {
                String::new()
            } else {
                args.initial_effort_name
                    .clone()
                    .unwrap_or_else(|| "auto".into())
            };
        }
        #[cfg(unix)]
        if refresh_saved_settings || saved_settings_path.as_ref() != Some(&session_path) {
            let switched = saved_settings_path.as_ref() != Some(&session_path);
            let activation = match read_saved_activation(
                &session,
                &initial_saved_settings,
                &model_name,
                switched,
            ) {
                Ok(activation) => activation,
                Err(error) => {
                    fatal_message = Some(error);
                    break 'outer ExitCode::FAILURE;
                }
            };
            if let Some(activation) = activation {
                if let Err(error) = apply_saved_binding(
                    &activation,
                    &mut effective_saved_settings,
                    &provider_name,
                    args.provider.as_ref(),
                    &mut model_name,
                ) {
                    fatal_message = Some(error);
                    break 'outer ExitCode::FAILURE;
                }
                apply_saved_draft(
                    activation,
                    SavedDraftTarget {
                        input_buffer: &mut input_buffer,
                        input_cursor: &mut input_cursor,
                        input_history: &mut input_history,
                        saved_text: &mut draft_saved_text,
                        candidate: &mut draft_candidate,
                        #[cfg(target_os = "linux")]
                        changed_at: &mut draft_changed_at,
                    },
                );
            }
            saved_settings_path = Some(session_path.clone());
            refresh_saved_settings = false;
            tool_memory_path = None;
        }
        attach_process_services(
            &mut session,
            ProcessServices {
                decider: &args.decider,
                tier_decider: &args.tier_decider,
                light_model: &args.mcp_selection_provider,
                approval_auto: args.approval_auto,
                setting: &setting,
                computer: &computer,
            },
        );
        // A queued submission uses the same slash-command/prompt path as an
        // idle Enter press. Preserve anything the user has started typing
        // since queueing it, then restore that draft before the queued item
        // begins its turn.
        let mut draft_after_queued_dispatch = None;
        #[cfg(unix)]
        if active_instruction.is_none() && !queued_inputs_paused {
            active_instruction = queued_instructions.pop_front();
        }
        #[cfg(unix)]
        let instruction_text = active_instruction
            .as_ref()
            .map(|item| item.text().to_owned());
        #[cfg(not(unix))]
        let instruction_text: Option<String> = None;
        let dispatching_queued_input = if let Some(queued) = instruction_text {
            draft_after_queued_dispatch = Some((std::mem::take(&mut input_buffer), input_cursor));
            input_buffer = queued;
            input_cursor = input_buffer.len();
            true
        } else {
            match promote_queued_input(
                &mut queued_inputs,
                queued_inputs_paused,
                &mut input_buffer,
                &mut input_cursor,
            ) {
                Ok(Some(draft)) => {
                    draft_after_queued_dispatch = Some(draft);
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    status = Status::Error(error.into());
                    false
                }
            }
        };
        if redact_inline_secret_value(&mut input_buffer, &mut input_cursor, &mut input_history) {
            status = Status::Error("Secrets cannot be entered in the normal input field".into());
        }
        #[cfg(unix)]
        let memory_activation =
            session_activation::MemoryActivation::from_saved(&effective_saved_settings);
        #[cfg(not(unix))]
        let memory_activation = session_activation::MemoryActivation::from_runtime(
            args.tool_memory,
            args.tool_memory_embedding.clone(),
            args.compact_at,
            args.remember,
        );
        match apply_tool_memory(
            &mut session,
            &args.cwd,
            &args.state_dir,
            &session_path,
            tool_memory_path.as_deref(),
            &memory_activation,
        ) {
            Ok(true) => {
                tool_memory_path = Some(session_path.clone());
            }
            Ok(false) => {}
            Err(error) => {
                fatal_message = Some(error);
                break 'outer ExitCode::FAILURE;
            }
        }
        let mut cumulative_usage = usage_meter.snapshot().usage;
        if let Err(error) = apply_compaction_and_memory(
            &mut session,
            &args.cwd,
            &args.state_dir,
            &session_path,
            &memory_activation,
        ) {
            fatal_message = Some(error);
            break 'outer ExitCode::FAILURE;
        }
        let mut colocated = vec![args.provider.clone()];
        colocated.extend(args.mcp_selection_provider.iter().cloned());
        let unknown_resources = session.strict_history.is_some();
        args.decision_colocation.replace(
            colocated,
            unknown_resources || memory_activation.unverified_model_allocation(),
        );
        // Recomputed every draw from the live buffer, so the popup tracks
        // each keystroke — not just the moment `/` was first typed.
        #[cfg(unix)]
        let has_pending_inputs = !queued_inputs.is_empty()
            || !queued_instructions.is_empty()
            || active_instruction.is_some();
        #[cfg(not(unix))]
        let has_pending_inputs = !queued_inputs.is_empty();
        let suggestions =
            slash_suggestions(&input_buffer, &input_history, dispatching_queued_input);
        if selected_suggestion >= suggestions.len() {
            selected_suggestion = 0;
        }
        // Print anything new since the last iteration (typically the
        // previous turn's assistant reply, or a `/resume`/`/new`/`/fork`
        // reset) before redrawing the small footer viewport — printing is
        // one-shot and must happen before the footer draw so the newly
        // printed lines appear above it, not interleaved mid-frame.
        conversation_view.append_new_history(&session, &local_lines);
        if draw_chat_frame(
            &mut terminal.borrow_mut(),
            &input_buffer,
            input_cursor,
            suggestions.len(),
            |f| {
                draw_frame(
                    f,
                    &conversation_view.history,
                    &mut conversation_view.history_wrap_cache,
                    conversation_view.scroll_offset,
                    &input_buffer,
                    input_cursor,
                    &status,
                    &render::HeaderInfo {
                        provider_name: &provider_name,
                        model_name: &model_name,
                        usage: cumulative_usage,
                        cwd: &cwd_display,
                        cwd_short: &cwd_footer_display,
                        effort_name: &effort_name,
                    },
                    &suggestions,
                    selected_suggestion,
                    &conversation_view.selection,
                )
            },
        )
        .is_err()
        {
            break ExitCode::FAILURE;
        }

        // Reads raw terminal events until one actually needs a redraw or
        // key handling — a bare mouse-move (`MouseEventKind::Moved`, which
        // `EnableMouseCapture`'s all-motion reporting fires on *any*
        // mouse movement over the terminal, not just a drag) is consumed
        // right here without ever reaching the outer loop's top-of-frame
        // redraw. Every other mouse kind still falls through to `continue
        // 'outer` exactly as before, so the redraw timing for
        // scroll/select/copy is unchanged.
        let event = if dispatching_queued_input {
            ratatui::crossterm::event::Event::Key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ))
        } else {
            loop {
                if !is_secret_command(&input_buffer)
                    && file_input::may_have_tail(&input_buffer, input_cursor)
                    && !ratatui::crossterm::event::poll(Duration::from_millis(40)).unwrap_or(true)
                    && file_input::normalize_tail(&mut input_buffer, &mut input_cursor)
                {
                    status = Status::Notice("File link inserted".into());
                    continue 'outer;
                }
                #[cfg(target_os = "linux")]
                let candidate = match linux_read_idle_event_with(
                    |duration| {
                        let ready = ratatui::crossterm::event::poll(duration)?;
                        if session.persistence.is_some() {
                            let pending = input_history.pending_draft(&input_buffer);
                            if pending != draft_candidate {
                                draft_candidate = pending.to_owned();
                                draft_changed_at = Instant::now();
                            }
                            if !ready
                                && draft_candidate != draft_saved_text
                                && draft_changed_at.elapsed() >= Duration::from_millis(350)
                            {
                                if let Err(error) = draft::save(
                                    &session,
                                    &initial_saved_settings,
                                    rewind_workspace.as_ref().expect("startup owner"),
                                    &management_effects,
                                    &draft_candidate,
                                ) {
                                    fatal_message = Some(format!("Cannot save the draft: {error}"));
                                    return Err(std::io::Error::other(error));
                                }
                                draft_saved_text = draft_candidate.clone();
                            }
                        }
                        Ok(ready)
                    },
                    ratatui::crossterm::event::read,
                    || signal::pending_linux_process_shutdown().is_some(),
                ) {
                    Ok(Some(event)) => event,
                    Ok(None) | Err(_) if linux_pending_exit_code().is_some() => {
                        break 'outer linux_pending_exit_code().unwrap_or(ExitCode::FAILURE);
                    }
                    Ok(None) | Err(_) => break 'outer ExitCode::FAILURE,
                };
                #[cfg(not(target_os = "linux"))]
                let candidate = match crate::terminal::read_event() {
                    Ok(e) => e,
                    Err(_) => break 'outer ExitCode::FAILURE,
                };
                #[cfg(target_os = "macos")]
                if !matches!(&candidate, ratatui::crossterm::event::Event::Mouse(_)) {
                    conversation_view.file_click.cancel();
                }
                if let ratatui::crossterm::event::Event::Mouse(mouse) = &candidate {
                    exit_confirmation.cancel();
                    #[cfg(target_os = "macos")]
                    if let Some(notice) = reveal_history_file(
                        &mut conversation_view,
                        *mouse,
                        &args.cwd,
                        ratatui::layout::Rect::new(
                            0,
                            0,
                            terminal_width(&terminal),
                            area_height_for_history(
                                render::footer_height(suggestions.len()),
                                terminal_height(&terminal),
                            ),
                        ),
                    ) {
                        status = notice;
                    }
                    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
                    if screen_selection::link_modifier(mouse.modifiers)
                        && matches!(
                            mouse.kind,
                            MouseEventKind::Down(MouseButton::Left)
                                | MouseEventKind::Drag(MouseButton::Left)
                                | MouseEventKind::Up(MouseButton::Left)
                        )
                    {
                        conversation_view.selection = None;
                        continue 'outer;
                    }
                    match mouse.kind {
                        MouseEventKind::Moved => continue,
                        MouseEventKind::ScrollUp => {
                            conversation_view.scroll_offset = conversation_view
                                .scroll_offset
                                .saturating_add(MOUSE_SCROLL_LINES);
                        }
                        MouseEventKind::ScrollDown => {
                            conversation_view.scroll_offset = conversation_view
                                .scroll_offset
                                .saturating_sub(MOUSE_SCROLL_LINES);
                        }
                        MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                            let history_area_height = area_height_for_history(
                                render::footer_height(suggestions.len()),
                                terminal_height(&terminal),
                            );
                            if mouse.row < history_area_height {
                                let width = terminal_width(&terminal);
                                let wrapped =
                                    render::wrap_history_lines(&conversation_view.history, width);
                                let window = render::visible_history_window(
                                    wrapped.len(),
                                    conversation_view.scroll_offset,
                                    history_area_height as usize,
                                );
                                let pos = selection::text_pos_from_screen(
                                    &wrapped,
                                    window,
                                    ratatui::layout::Rect::new(0, 0, width, history_area_height),
                                    mouse.row,
                                    mouse.column,
                                );
                                conversation_view.selection = Some(selection::Selection {
                                    anchor: pos,
                                    cursor: pos,
                                    dragging: true,
                                });
                            }
                        }
                        MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left) => {
                            if let Some(sel) = conversation_view.selection.as_mut()
                                && sel.dragging
                            {
                                let history_area_height = area_height_for_history(
                                    render::footer_height(suggestions.len()),
                                    terminal_height(&terminal),
                                );
                                let width = terminal_width(&terminal);
                                let wrapped =
                                    render::wrap_history_lines(&conversation_view.history, width);
                                let window = render::visible_history_window(
                                    wrapped.len(),
                                    conversation_view.scroll_offset,
                                    history_area_height as usize,
                                );
                                sel.cursor = selection::text_pos_from_screen(
                                    &wrapped,
                                    window,
                                    ratatui::layout::Rect::new(0, 0, width, history_area_height),
                                    mouse.row,
                                    mouse.column,
                                );
                                if mouse.row == 0 {
                                    conversation_view.scroll_offset =
                                        conversation_view.scroll_offset.saturating_add(1);
                                } else if mouse.row.saturating_add(1) >= history_area_height {
                                    conversation_view.scroll_offset =
                                        conversation_view.scroll_offset.saturating_sub(1);
                                }
                            }
                        }
                        MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left) => {
                            if let Some(sel) = conversation_view.selection.as_mut()
                                && sel.dragging
                            {
                                sel.dragging = false;
                                let width = terminal_width(&terminal);
                                let wrapped =
                                    render::wrap_history_lines(&conversation_view.history, width);
                                let text = selection::extract_text(&wrapped, sel);
                                if !text.is_empty() {
                                    #[cfg(unix)]
                                    {
                                        status = match local_actions::handle_with_selection(
                                            &slash::Action::Copy,
                                            &terminal,
                                            &mut key_reader,
                                            &mut session,
                                            rewind_workspace.as_ref().expect("startup owner"),
                                            &management_effects,
                                            &args.cwd,
                                            has_pending_inputs,
                                            Some(&text),
                                            clipboard::copy_to_clipboard,
                                        ) {
                                            Ok(lines) => Status::Notice(
                                                lines
                                                    .iter()
                                                    .map(|l| l.to_string())
                                                    .collect::<Vec<_>>()
                                                    .join(" / "),
                                            ),
                                            Err(error) => Status::Error(error),
                                        };
                                    }
                                    #[cfg(not(unix))]
                                    {
                                        status = Status::Notice(apply_selection_copy(
                                            &text,
                                            clipboard::copy_to_clipboard,
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue 'outer;
                }
                break candidate;
            }
        };
        #[cfg(target_os = "linux")]
        if let Some(code) = linux_pending_exit_code() {
            break 'outer code;
        }
        if let ratatui::crossterm::event::Event::Resize(_, _) = &event {
            conversation_view.selection = None;
        }
        let key = match event {
            ratatui::crossterm::event::Event::Key(key) => key,
            ratatui::crossterm::event::Event::Paste(mut text) => {
                let result = apply_composer_paste(
                    &mut input_buffer,
                    &mut input_cursor,
                    &mut input_history,
                    &text,
                );
                zeroize::Zeroize::zeroize(&mut text);
                match result {
                    Ok(true) => status = Status::Notice("File link inserted".into()),
                    Err(error) => status = Status::Error(error.into()),
                    _ => {}
                }
                continue;
            }
            _ => continue,
        };
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press
            && !is_secret_command(&input_buffer)
            && matches!(
                key.code,
                ratatui::crossterm::event::KeyCode::Backspace
                    | ratatui::crossterm::event::KeyCode::Delete
                    | ratatui::crossterm::event::KeyCode::Left
                    | ratatui::crossterm::event::KeyCode::Right
                    | ratatui::crossterm::event::KeyCode::Enter
                    | ratatui::crossterm::event::KeyCode::Up
                    | ratatui::crossterm::event::KeyCode::Down
            )
        {
            file_input::normalize_tail(&mut input_buffer, &mut input_cursor);
        }
        if secret_value_key_is_blocked(&input_buffer, &key) {
            status = Status::Error("Secrets cannot be entered in the normal input field".into());
            continue;
        }
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press
            && !(key.code == ratatui::crossterm::event::KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL))
        {
            exit_confirmation.cancel();
        }

        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::KeyCode;
            if key.code == KeyCode::Esc {
                if !input_buffer.is_empty() || has_pending_inputs || dispatching_queued_input {
                    rewind_gesture.cancel();
                    status = Status::Notice(
                        "Clear the input field and queued input before rewinding".into(),
                    );
                    continue;
                }
                if !rewind_gesture.press(Instant::now()) {
                    status = Status::Notice("Press Esc again to open rewind candidates".into());
                    continue;
                }
                #[cfg(unix)]
                {
                    let Some(owner) = rewind_workspace.as_ref() else {
                        status = Status::Notice(rewind_unavailable_reason.clone());
                        continue;
                    };
                    if let Err(error) = ensure_rewind_unblocked(
                        &polaris_core::project::resolve_root(&args.cwd),
                        &management_effects,
                    ) {
                        status = Status::Error(error);
                        continue;
                    }
                    let picked_rewind = match select_rewind_boundary(
                        &terminal,
                        &mut key_reader,
                        &session,
                        owner,
                        &rewind_unavailable_reason,
                    ) {
                        Ok(selection) => selection,
                        Err(error) => {
                            status =
                                Status::Error(format!("Cannot verify rewind candidates: {error}"));
                            continue;
                        }
                    };
                    let Some((boundary, expected_post, target_pre)) = picked_rewind else {
                        status = Status::Idle;
                        continue;
                    };
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    match perform_rewind(
                        &session,
                        &boundary,
                        (&expected_post, &target_pre),
                        owner,
                        &management_effects,
                        &args,
                        mcp_ceiling,
                    )
                    .await
                    {
                        Ok(activation) => {
                            session = activation.session;
                            session_path = activation.session_path;
                            meta_path = session_path.with_extension("meta.json");
                            session_started_at_millis = now_millis();
                            effective_skills = activation.skills;
                            effective_agent_types = activation.agent_types;
                            mcp_host = Some(activation.mcp_host);
                            mcp_setup = Some(activation.mcp_setup);
                            tool_memory_path = Some(session_path.clone());
                            input_history = input::History::new();
                            history_compacted_this_turn = false;
                            local_lines.clear();
                            let width = terminal
                                .borrow()
                                .size()
                                .map(|size| size.width)
                                .unwrap_or(80);
                            conversation_view.reset(
                                &render::HeaderInfo {
                                    provider_name: &provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            status = Status::Notice(format!(
                                "Rewound to before input \"{}\". The original conversation is saved.",
                                boundary
                                    .user_message_preview
                                    .as_deref()
                                    .unwrap_or("unknown")
                            ));
                        }
                        Err(error) if error.fatal => {
                            fatal_message = Some(error.message);
                            break 'outer ExitCode::FAILURE;
                        }
                        Err(error) => status = Status::Error(error.message),
                    }
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    continue;
                }
                #[cfg(not(unix))]
                {
                    status =
                        Status::Notice("File rewind is unavailable in this environment".into());
                    continue;
                }
            }
            rewind_gesture.cancel();
        }

        // `/review` isn't a local command like the others — it expands
        // into a real user message and falls through to the normal model
        // turn below, so both places that can resolve it (accepting a
        // highlighted suggestion, and typing the full name) funnel into
        // this instead of calling `apply_slash_action`.
        let mut review_text: Option<String> = None;

        // While the popup is open, Up/Down move the highlight and Enter
        // accepts whichever row is highlighted — mirroring codex's own
        // picker. Typing the full command name and pressing Enter still
        // works too, since at that point it's the (only) highlighted row.
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press && !suggestions.is_empty() {
            match slash_completion_action(&suggestions, &mut selected_suggestion, key) {
                SlashCompletionAction::Navigate => continue,
                SlashCompletionAction::Submit(command) => {
                    let action = slash::action_for(command);
                    if let Err(error) = require_action_owner(&action) {
                        status = Status::Error(error.into());
                        continue;
                    }
                    #[cfg(unix)]
                    {
                        last_submission_was_local = true;
                    }
                    #[cfg(unix)]
                    if let Some(error) = management_clearance_error(
                        &polaris_core::project::resolve_root(&args.cwd),
                        &management_effects,
                        &action,
                    ) {
                        status = Status::Error(error);
                        continue;
                    }
                    // Completion may execute a different name than the typed
                    // prefix. Record that command through the same guards as
                    // normal Enter, preserving hidden drafts for new/quit.
                    record_submission_before_dispatch(
                        &mut input_history,
                        &format!("/{command}"),
                        dispatching_queued_input,
                    );
                    let pending_input = std::mem::take(&mut input_buffer);
                    let pending_cursor = input_cursor;
                    input_cursor = 0;
                    selected_suggestion = 0;
                    #[cfg(unix)]
                    if let Some(result) = command_dispatch::dispatch(
                        &action,
                        &terminal,
                        &mut key_reader,
                        &mut session,
                        (
                            &initial_saved_settings,
                            args.provider.as_ref(),
                            args.history_backend_factory.as_ref(),
                        ),
                        (
                            rewind_workspace.as_ref().expect("startup owner"),
                            &management_effects,
                        ),
                        (&args.cwd, &args.state_dir),
                        memory_activation.embedding(),
                        (args.mcp_external, approval_policy),
                        has_pending_inputs,
                    )
                    .await
                    {
                        local_lines.extend(result.lines);
                        refresh_saved_settings |= result.settings_changed;
                        if result.invalidate_mcp_prompt {
                            pending_mcp_prompt = None;
                            mcp_prompt_requires_reselect = false;
                        }
                        status = result.status;
                        continue;
                    }
                    match action {
                        #[cfg(target_os = "linux")]
                        slash::Action::WorkspaceControl(request) => {
                            let (verb, extra) = request.split_once(' ').unwrap_or((&request, ""));
                            let result = match workspace_commands.as_ref() {
                                Ok(owner) => match verb {
                                    "setting" => owner.setting(
                                        extra,
                                        &terminal,
                                        &mut key_reader,
                                        rewind_workspace.as_ref().expect("startup owner"),
                                        &management_effects,
                                        has_pending_inputs,
                                    ),
                                    _ => Err("No target operation".into()),
                                },
                                Err(error) => Err(error.clone()),
                            };
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    status = Status::Notice("Read result displayed".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(target_os = "linux")]
                        slash::Action::Decided(request) if request.starts_with("add ") => {
                            let result = workspace_commands
                                .as_ref()
                                .map_err(Clone::clone)
                                .and_then(|owner| {
                                    owner.decided(
                                        request.trim_start_matches("add "),
                                        &format!("{}", now_millis()),
                                        &terminal,
                                        &mut key_reader,
                                        rewind_workspace.as_ref().expect("startup owner"),
                                        &management_effects,
                                        has_pending_inputs,
                                    )
                                });
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    status =
                                        Status::Notice("Decision record result displayed".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(unix)]
                        slash::Action::Clear
                        | slash::Action::Init
                        | slash::Action::Export(_)
                        | slash::Action::Copy => {
                            let operation = &action;
                            let result = local_actions::handle(
                                operation,
                                &terminal,
                                &mut key_reader,
                                &mut session,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                &args.cwd,
                                has_pending_inputs,
                                clipboard::copy_to_clipboard,
                            );
                            match result {
                                Ok(lines) => {
                                    if matches!(operation, slash::Action::Clear)
                                        && lines.iter().any(|line| {
                                            line.to_string().starts_with("T08 complete")
                                        })
                                    {
                                        local_lines.clear();
                                        let width =
                                            terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                        conversation_view.reset(
                                            &render::HeaderInfo {
                                                provider_name: &provider_name,
                                                model_name: &model_name,
                                                usage: cumulative_usage,
                                                cwd: &cwd_display,
                                                cwd_short: &cwd_footer_display,
                                                effort_name: &effort_name,
                                            },
                                            width,
                                        );
                                    }
                                    local_lines.extend(lines);
                                    status = Status::Notice(
                                        "Management operation result displayed".into(),
                                    );
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(unix)]
                        slash::Action::RuntimeControl(request) => {
                            if let Some(result) = handle_queued_control(
                                &request,
                                &terminal,
                                &mut key_reader,
                                &session,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                &args.state_dir,
                                &mut queued_instructions,
                                &mut active_instruction,
                            ) {
                                match result {
                                    Ok(lines) => {
                                        local_lines.extend(
                                            lines.into_iter().map(ratatui::text::Line::from),
                                        );
                                        status = Status::Notice(
                                            "Admitted input result displayed".into(),
                                        );
                                    }
                                    Err(error) => {
                                        queued_inputs_paused = true;
                                        status = Status::Error(error);
                                    }
                                }
                                continue;
                            }

                            if request == "roles" || request.starts_with("roles ") {
                                match role_commands::handle(
                                    &request,
                                    &terminal,
                                    &mut key_reader,
                                    &session,
                                    &initial_saved_settings,
                                    rewind_workspace.as_ref().expect("startup owner"),
                                    &management_effects,
                                    args.provider.as_ref(),
                                    &effective_agent_types,
                                    has_pending_inputs,
                                ) {
                                    Ok(lines) => {
                                        local_lines.extend(lines);
                                        refresh_saved_settings = true;
                                        status =
                                            Status::Notice("Role binding result displayed".into());
                                    }
                                    Err(error) => status = Status::Error(error),
                                }
                                continue;
                            }
                            if request.starts_with("sandbox ") && request != "sandbox status" {
                                match sandbox_commands::handle(
                                    &request,
                                    &terminal,
                                    &mut key_reader,
                                    &session,
                                    rewind_workspace.as_ref().expect("startup owner"),
                                    &management_effects,
                                    &args.sandbox,
                                    has_pending_inputs,
                                ) {
                                    Ok(lines) => {
                                        local_lines.extend(lines);
                                        status = Status::Notice(
                                            "Sandbox execution result displayed".into(),
                                        );
                                    }
                                    Err(error) => status = Status::Error(error),
                                }
                                continue;
                            }
                            let result = handle_runtime_command(
                                &request,
                                session.delegation_control.as_ref(),
                                session.persistence.as_ref(),
                                &args.state_dir,
                                &effective_agent_types,
                                &args.sandbox,
                                approval_policy,
                                &mut next_max_turns,
                                args.max_turns,
                                &conversation_view,
                            )
                            .await;
                            match result {
                                Ok(lines) => {
                                    local_lines
                                        .extend(lines.into_iter().map(ratatui::text::Line::from));
                                    status = Status::Notice("Execution state displayed".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(unix)]
                        slash::Action::Account(_) | slash::Action::Logout => {
                            let request = match &action {
                                slash::Action::Account(request) => request.as_str(),
                                _ => "logout",
                            };
                            let result = if request.is_empty() || request == "list" {
                                Ok(())
                            } else {
                                session.check_persistence().map_err(|e| e.to_string())
                            };
                            let result = match result {
                                Ok(()) => {
                                    account::handle(
                                        request,
                                        &terminal,
                                        &mut key_reader,
                                        rewind_workspace.as_ref().expect("startup owner"),
                                        &management_effects,
                                        args.account_binding.as_ref(),
                                        has_pending_inputs,
                                    )
                                    .await
                                }
                                Err(error) => Err(error),
                            };
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    status = Status::Notice(
                                        "Authentication operation result displayed".into(),
                                    );
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        slash::Action::Status => {
                            local_lines.extend(status_lines(StatusSnapshot {
                                session: &session,
                                session_path: Some(&session_path),
                                accounts: account_list_lines(),
                                #[cfg(unix)]
                                binding: args.account_binding.as_deref(),
                                provider_name: &provider_name,
                                model_name: &model_name,
                                usage_report: usage_meter.snapshot(),
                                sandbox: &args.sandbox,
                                approval: approval_policy,
                                startup_approval: args.approval_policy,
                                mcp_external: args.mcp_external,
                                audit_path: Some(&args.audit_path),
                                turn_authority_present: args.turn_authority.is_some(),
                                fixed_effort: args.initial_effort_name.as_deref(),
                                answer_finalizer: args.answer_finalizer.as_deref(),
                                #[cfg(unix)]
                                project_root: &polaris_core::project::resolve_root(&args.cwd),
                                #[cfg(unix)]
                                management_effects: &management_effects,
                                #[cfg(unix)]
                                recovery_issue: workflow_recovery_issue.as_deref(),
                            }));
                            local_lines.extend(
                                polaris_core::decision_diagnostics::display(
                                    &args.decision_configuration,
                                    args.decider.as_deref(),
                                )
                                .into_iter()
                                .map(ratatui::text::Line::from),
                            );
                            status = Status::Notice("Status displayed".into());
                            continue;
                        }
                        slash::Action::Doctor => {
                            local_lines.extend(doctor_lines(
                                &provider_name,
                                &model_name,
                                &args.cwd,
                                &args.sandbox,
                                approval_policy,
                                args.mcp_external,
                            ));
                            #[cfg(unix)]
                            local_lines.extend(management_effect_diagnostic(
                                &polaris_core::project::resolve_root(&args.cwd),
                                &management_effects,
                                workflow_recovery_issue.as_deref(),
                            ));
                            local_lines
                                .extend(cat_diagnostic_lines(args.answer_finalizer.as_deref()));
                            #[cfg(unix)]
                            local_lines.extend(rewind_candidate_diagnostic(
                                rewind_workspace.as_ref(),
                                &args.sessions_dir,
                                &args.cwd,
                            ));
                            status = Status::Notice("Diagnostic results displayed".into());
                            continue;
                        }
                        slash::Action::Review(extra) => {
                            review_text = Some(review_prompt(&extra));
                        }
                        slash::Action::Resume => {
                            #[cfg(target_os = "linux")]
                            if let Some(code) = linux_pending_exit_code() {
                                break 'outer code;
                            }
                            let previous_session_path = session_path.clone();
                            handle_resume(
                                &terminal,
                                &mut key_reader,
                                &args.sessions_dir,
                                &args.state_dir,
                                &cwd_display,
                                &args.workflow_config,
                                args.workflow_mode_overridden,
                                &effective_skills,
                                args.history_backend_factory.as_ref(),
                                #[cfg(unix)]
                                rewind_workspace.as_ref(),
                                #[cfg(unix)]
                                Some(&management_effects),
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                                &mut local_lines,
                            )
                            .await;
                            #[cfg(target_os = "linux")]
                            if let Some(code) = linux_pending_exit_code() {
                                break 'outer code;
                            }
                            clear_mcp_prompt_on_session_change(
                                &previous_session_path,
                                &session_path,
                                &mut pending_mcp_prompt,
                                &mut mcp_prompt_requires_reselect,
                            );
                            // A resumed session may be the same length as
                            // (or longer than) what's already printed but
                            // still a genuinely *different* conversation —
                            // the length-shrunk check inside
                            // `append_new_history` can't detect that case,
                            // so reset explicitly here.
                            let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                            reset_conversation_view_after_resume(
                                &mut conversation_view,
                                &render::HeaderInfo {
                                    provider_name: &provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            continue;
                        }
                        slash::Action::New => {
                            let previous_session_path = session_path.clone();
                            #[cfg(unix)]
                            let switched = handle_managed_new_session(
                                &args.sessions_dir,
                                &args.state_dir,
                                rewind_workspace.as_ref().expect("Unix workspace owner"),
                                &management_effects,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                                &mut local_lines,
                            );
                            #[cfg(not(unix))]
                            let switched = handle_new_session(
                                &args.sessions_dir,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                                &mut local_lines,
                            );
                            if !switched {
                                restore_rejected_new_submission(
                                    pending_input,
                                    pending_cursor,
                                    false,
                                    &mut input_buffer,
                                    &mut input_cursor,
                                    &mut queued_inputs,
                                    &mut queued_inputs_paused,
                                );
                                continue;
                            }
                            queued_inputs_paused = false;
                            record_submission(&mut input_history, "/new", dispatching_queued_input);
                            clear_mcp_prompt_on_session_change(
                                &previous_session_path,
                                &session_path,
                                &mut pending_mcp_prompt,
                                &mut mcp_prompt_requires_reselect,
                            );
                            let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                            conversation_view.reset(
                                &render::HeaderInfo {
                                    provider_name: &provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            continue;
                        }
                        slash::Action::Fork => {
                            let previous_session_path = session_path.clone();
                            #[cfg(unix)]
                            handle_managed_fork(
                                &args.sessions_dir,
                                &cwd_display,
                                &args.state_dir,
                                rewind_workspace.as_ref().expect("Unix workspace owner"),
                                &management_effects,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                            );
                            #[cfg(not(unix))]
                            handle_fork(
                                &args.sessions_dir,
                                &cwd_display,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                            );
                            clear_mcp_prompt_on_session_change(
                                &previous_session_path,
                                &session_path,
                                &mut pending_mcp_prompt,
                                &mut mcp_prompt_requires_reselect,
                            );
                            let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                            conversation_view.reset(
                                &render::HeaderInfo {
                                    provider_name: &provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            continue;
                        }
                        slash::Action::Permissions => {
                            handle_permissions(
                                &terminal,
                                &mut key_reader,
                                &mut approval_policy,
                                args.approval_policy,
                                &args.sandbox,
                                &mut status,
                            );
                            continue;
                        }
                        slash::Action::Model => {
                            #[cfg(target_os = "linux")]
                            if let Some(code) = linux_pending_exit_code() {
                                break 'outer code;
                            }
                            handle_model(
                                &terminal,
                                &mut key_reader,
                                &mut args.provider,
                                &cloud_provider,
                                &usage_meter,
                                &local_router,
                                &mut provider_name,
                                &cloud_provider_name,
                                &mut model_name,
                                &cloud_effort_name,
                                &mut effort_name,
                                &mut status,
                            )
                            .await;
                            #[cfg(target_os = "linux")]
                            if let Some(code) = linux_pending_exit_code() {
                                break 'outer code;
                            }
                            continue;
                        }
                        slash::Action::Skills => {
                            run_skills_picker(&terminal, &mut key_reader, &effective_skills);
                            status = Status::Idle;
                            continue;
                        }
                        #[cfg(unix)]
                        action @ (slash::Action::Phase(_) | slash::Action::Workflow(_)) => {
                            handle_managed_workflow_action(
                                &terminal,
                                &mut key_reader,
                                action,
                                &mut session,
                                &effective_skills,
                                rewind_workspace.as_ref().expect("Unix workspace owner"),
                                &management_effects,
                                &args.state_dir,
                                has_pending_inputs || dispatching_queued_input,
                                &mut status,
                            );
                            continue;
                        }
                        #[cfg(any(target_os = "linux", target_os = "macos"))]
                        slash::Action::Sync(requested) => {
                            if is_c10_sync_request(&requested) {
                                #[cfg(target_os = "macos")]
                                match session
                                    .check_persistence()
                                    .map_err(|error| error.to_string())
                                    .and_then(|()| {
                                        start_c10_sync_action(
                                            &args.cwd,
                                            &args.state_dir,
                                            &requested,
                                            has_pending_inputs || dispatching_queued_input,
                                        )
                                    }) {
                                    Ok(job) => {
                                        pending_c10_job = Some(job);
                                        status = Status::Notice("C10 operation started".into());
                                    }
                                    Err(error) => {
                                        record_c10_result(Err(error), &mut status, &mut local_lines)
                                    }
                                }
                                #[cfg(target_os = "linux")]
                                record_c10_result(
                                    c10_unavailable_action(
                                        &requested,
                                        has_pending_inputs || dispatching_queued_input,
                                    ),
                                    &mut status,
                                    &mut local_lines,
                                );
                                continue;
                            }
                            handle_sync_action(
                                &terminal,
                                &mut key_reader,
                                &args.cwd,
                                &args.state_dir,
                                &requested,
                                has_pending_inputs || dispatching_queued_input,
                                &session,
                                &mut status,
                                &mut local_lines,
                            );
                            continue;
                        }
                        #[cfg(target_os = "linux")]
                        slash::Action::Apply(requested) => {
                            handle_source_apply_action(
                                &terminal,
                                &mut key_reader,
                                &args,
                                audit.clone(),
                                &mut source_frontend,
                                &requested,
                                has_pending_inputs || dispatching_queued_input,
                                &session,
                                &mut status,
                                &mut local_lines,
                            )
                            .await;
                            continue;
                        }
                        slash::Action::Compact => {
                            // A real summarization call can take tens of
                            // seconds — show something before awaiting it
                            // rather than leaving the last drawn frame on
                            // screen, which reads as a hang (same
                            // "set status, force one immediate draw, then
                            // do the slow thing" shape as the Ctrl+O
                            // mid-turn confirmation above).
                            status = Status::Notice("compacting...".to_string());
                            if draw_chat_frame(
                                &mut terminal.borrow_mut(),
                                &input_buffer,
                                input_cursor,
                                0,
                                |f| {
                                    draw_frame(
                                        f,
                                        &conversation_view.history,
                                        &mut conversation_view.history_wrap_cache,
                                        conversation_view.scroll_offset,
                                        &input_buffer,
                                        input_cursor,
                                        &status,
                                        &render::HeaderInfo {
                                            provider_name: &provider_name,
                                            model_name: &model_name,
                                            usage: cumulative_usage,
                                            cwd: &cwd_display,
                                            cwd_short: &cwd_footer_display,
                                            effort_name: &effort_name,
                                        },
                                        &[],
                                        0,
                                        &conversation_view.selection,
                                    )
                                },
                            )
                            .is_err()
                            {
                                break ExitCode::FAILURE;
                            }
                            #[cfg(target_os = "linux")]
                            if let Some(code) = linux_pending_exit_code() {
                                break 'outer code;
                            }
                            #[cfg(unix)]
                            let _compact_persistence_failed = match local_actions::compact(
                                &terminal,
                                &mut key_reader,
                                args.provider.as_ref(),
                                &mut session,
                                &session_path,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                &mut status,
                                &mut cumulative_usage,
                                has_pending_inputs,
                            )
                            .await
                            {
                                Ok(_) => false,
                                Err(error) => {
                                    status = Status::Error(error);
                                    true
                                }
                            };
                            #[cfg(not(unix))]
                            let _compact_persistence_failed = handle_compact(
                                args.provider.as_ref(),
                                &mut session,
                                &session_path,
                                &mut status,
                                &mut cumulative_usage,
                            )
                            .await;
                            #[cfg(target_os = "linux")]
                            if let Some(code) = linux_pending_exit_code() {
                                if _compact_persistence_failed {
                                    fatal_message = Some(
                                        "compacted history could not be persisted during shutdown"
                                            .into(),
                                    );
                                    break 'outer ExitCode::FAILURE;
                                }
                                break 'outer code;
                            }
                            conversation_view.mark_messages_printed(session.messages.len());
                            continue;
                        }
                        action => {
                            // `apply_slash_action` clears `session.messages`/
                            // `local_lines` for `Clear` but has no access to
                            // `history`/`scroll_offset` to re-seed the header
                            // and reset the scroll position — do that here
                            // instead of relying on `append_new_history`'s
                            // shrink-guard to (not) do it (see that
                            // function's doc comment).
                            let clear_view = matches!(action, slash::Action::Clear);
                            match apply_slash_action(
                                action,
                                &mut session,
                                &session_path,
                                &mut status,
                                &provider_name,
                                &model_name,
                                usage_meter.snapshot(),
                                &effective_skills,
                                &args.cwd,
                                &args.state_dir,
                                &args.sessions_dir,
                                args.mcp_external,
                                &mut local_lines,
                            ) {
                                SlashOutcome::Continue => {
                                    if clear_view {
                                        let width =
                                            terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                        conversation_view.reset(
                                            &render::HeaderInfo {
                                                provider_name: &provider_name,
                                                model_name: &model_name,
                                                usage: cumulative_usage,
                                                cwd: &cwd_display,
                                                cwd_short: &cwd_footer_display,
                                                effort_name: &effort_name,
                                            },
                                            width,
                                        );
                                    }
                                    continue;
                                }
                                SlashOutcome::Quit => {
                                    #[cfg(unix)]
                                    if !queued_instructions.is_empty()
                                        || active_instruction.is_some()
                                    {
                                        status = Status::Notice(
                                            "Process or cancel admitted instructions before exiting"
                                                .into(),
                                        );
                                        continue;
                                    }
                                    if refuse_quit_with_pending_work(
                                        &queued_inputs,
                                        &mut input_history,
                                        (&mut input_buffer, &mut input_cursor),
                                        draft_after_queued_dispatch.take(),
                                        &pending_input,
                                        dispatching_queued_input,
                                        &mut status,
                                    ) {
                                        continue;
                                    }
                                    break ExitCode::SUCCESS;
                                }
                                SlashOutcome::Fatal(msg) => {
                                    fatal_message = Some(msg);
                                    break 'outer ExitCode::FAILURE;
                                }
                                #[cfg(target_os = "linux")]
                                SlashOutcome::Shutdown(reason) => {
                                    break 'outer linux_signal_exit_code(reason);
                                }
                            }
                        }
                    }
                }
                SlashCompletionAction::PassThrough => {}
            }
        }

        // Up/Down recall previously submitted input, shell-history style —
        // matching ordinary terminals/shells. Only reachable once the
        // slash-popup above hasn't already claimed these keys (it
        // `continue`s whenever `suggestions` is non-empty).
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::Up => {
                    if let Some(recalled) = input_history.older(&input_buffer) {
                        input_buffer = recalled.to_string();
                        input_cursor = input_buffer.len();
                    }
                    continue;
                }
                KeyCode::Down => {
                    if let Some(recalled) = input_history.newer() {
                        input_buffer = recalled.to_string();
                        input_cursor = input_buffer.len();
                    }
                    continue;
                }
                _ => {}
            }
        }

        // PageUp/PageDown scroll the conversation history — not claimed by
        // the slash-popup or by input-history recall, so these are always
        // reachable when the input box has focus.
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::PageUp => {
                    conversation_view.scroll_offset =
                        conversation_view.scroll_offset.saturating_add(1);
                    continue;
                }
                KeyCode::PageDown => {
                    conversation_view.scroll_offset =
                        conversation_view.scroll_offset.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
        }

        // Ctrl+O copies the last reply to the clipboard — same hotkey and
        // behavior as codex's own `/copy`/`Ctrl+O` (see `slash::Action::Copy`).
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press
            && key.code == ratatui::crossterm::event::KeyCode::Char('o')
            && key
                .modifiers
                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
        {
            if let Err(error) = require_action_owner(&slash::Action::Copy) {
                status = Status::Error(error.into());
                continue;
            }
            #[cfg(unix)]
            {
                status = match local_actions::handle(
                    &slash::Action::Copy,
                    &terminal,
                    &mut key_reader,
                    &mut session,
                    rewind_workspace.as_ref().expect("startup owner"),
                    &management_effects,
                    &args.cwd,
                    has_pending_inputs,
                    clipboard::copy_to_clipboard,
                ) {
                    Ok(lines) => Status::Notice(
                        lines
                            .iter()
                            .map(|l| l.to_string())
                            .collect::<Vec<_>>()
                            .join(" / "),
                    ),
                    Err(error) => Status::Error(error),
                };
            }
            #[cfg(not(unix))]
            {
                status = Status::Notice(clipboard::copy_last_reply_with(
                    &session,
                    clipboard::copy_to_clipboard,
                ));
            }
            continue;
        }

        conversation_view.selection = None;

        let cursor_before_submit = input_cursor;
        let text = if let Some(t) = review_text {
            t
        } else {
            let action = match apply_admitted_key(&mut input_buffer, &mut input_cursor, key) {
                Ok(action) => action,
                Err(error) => {
                    status = Status::Error(error.into());
                    continue;
                }
            };
            let typed = match action {
                InputAction::Continue => {
                    #[cfg(unix)]
                    {
                        last_submission_was_local = false;
                    }
                    exit_confirmation.cancel();
                    selected_suggestion = 0;
                    if redact_inline_secret_value(
                        &mut input_buffer,
                        &mut input_cursor,
                        &mut input_history,
                    ) {
                        status = Status::Error(
                            "Secrets cannot be entered in the normal input field".into(),
                        );
                    }
                    continue;
                }
                InputAction::Quit => {
                    if exit_confirmation.press() {
                        break ExitCode::SUCCESS;
                    }
                    status = Status::Notice("Press Ctrl+C again to exit".to_string());
                    continue;
                }
                InputAction::Submit(text) if text.trim().is_empty() => {
                    exit_confirmation.cancel();
                    continue;
                }
                InputAction::Submit(mut text) => {
                    #[cfg(unix)]
                    {
                        last_submission_was_local = text.trim_start().starts_with('/');
                    }
                    exit_confirmation.cancel();
                    let local_secret_owner_available = {
                        #[cfg(unix)]
                        {
                            active_instruction.is_none()
                        }
                        #[cfg(not(unix))]
                        {
                            false
                        }
                    };
                    if refuse_secret_submission(&mut text, local_secret_owner_available) {
                        status = Status::Error(
                            "Secrets cannot be passed to normal or queued input".into(),
                        );
                        continue;
                    }
                    if !dispatching_queued_input && submission_resumes_queued_inputs(&text) {
                        queued_inputs_paused = false;
                    }
                    // In-flight submissions were recorded when Enter queued
                    // them. Recording the synthetic Enter again would add a
                    // second copy after any later queued entry.
                    record_submission_before_dispatch(
                        &mut input_history,
                        &text,
                        dispatching_queued_input,
                    );
                    conversation_view.scroll_offset = 0;
                    text
                }
            };
            if let Some((draft, cursor)) = draft_after_queued_dispatch.take() {
                input_buffer = draft;
                input_cursor = cursor;
            }
            // Slash commands are local: handled here and never reach
            // `session.messages`, the model, or the persisted session
            // file. Reached only for text the popup didn't already
            // resolve above — an exact-but-unmatched-by-Enter case can't
            // happen since Enter with any suggestion present is
            // intercepted, so this covers `/unknown-name` (no matches,
            // popup was empty) and plain text.
            let parsed_action = {
                #[cfg(unix)]
                if active_instruction.is_some() {
                    None
                } else {
                    slash::parse(&typed)
                }
                #[cfg(not(unix))]
                slash::parse(&typed)
            };
            #[cfg(unix)]
            if let Some(error) = parsed_action.as_ref().and_then(|action| {
                management_clearance_error(
                    &polaris_core::project::resolve_root(&args.cwd),
                    &management_effects,
                    action,
                )
            }) {
                if dispatching_queued_input {
                    queued_inputs.push_front(typed);
                    queued_inputs_paused = true;
                } else {
                    input_buffer = typed;
                    input_cursor = input_buffer.len();
                }
                status = Status::Error(error);
                continue;
            }
            #[cfg(unix)]
            if let Some(action) = parsed_action.as_ref()
                && let Some(result) = command_dispatch::dispatch(
                    action,
                    &terminal,
                    &mut key_reader,
                    &mut session,
                    (
                        &initial_saved_settings,
                        args.provider.as_ref(),
                        args.history_backend_factory.as_ref(),
                    ),
                    (
                        rewind_workspace.as_ref().expect("startup owner"),
                        &management_effects,
                    ),
                    (&args.cwd, &args.state_dir),
                    memory_activation.embedding(),
                    (args.mcp_external, approval_policy),
                    has_pending_inputs,
                )
                .await
            {
                local_lines.extend(result.lines);
                refresh_saved_settings |= result.settings_changed;
                if result.invalidate_mcp_prompt {
                    pending_mcp_prompt = None;
                    mcp_prompt_requires_reselect = false;
                }
                status = result.status;
                continue;
            }
            match parsed_action {
                #[cfg(target_os = "linux")]
                Some(slash::Action::WorkspaceControl(request)) => {
                    let (verb, extra) = request.split_once(' ').unwrap_or((&request, ""));
                    let result = match workspace_commands.as_ref() {
                        Ok(owner) => match verb {
                            "setting" => owner.setting(
                                extra,
                                &terminal,
                                &mut key_reader,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                has_pending_inputs,
                            ),
                            _ => Err("No target operation".into()),
                        },
                        Err(error) => Err(error.clone()),
                    };
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            status = Status::Notice("Read result displayed".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(target_os = "linux")]
                Some(slash::Action::Decided(request)) if request.starts_with("add ") => {
                    let result =
                        workspace_commands
                            .as_ref()
                            .map_err(Clone::clone)
                            .and_then(|owner| {
                                owner.decided(
                                    request.trim_start_matches("add "),
                                    &format!("{}", now_millis()),
                                    &terminal,
                                    &mut key_reader,
                                    rewind_workspace.as_ref().expect("startup owner"),
                                    &management_effects,
                                    has_pending_inputs,
                                )
                            });
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            status = Status::Notice("Decision record result displayed".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(unix)]
                Some(
                    slash::Action::Clear
                    | slash::Action::Init
                    | slash::Action::Export(_)
                    | slash::Action::Copy,
                ) => {
                    let operation = &parsed_action.as_ref().expect("local action");
                    let result = local_actions::handle(
                        operation,
                        &terminal,
                        &mut key_reader,
                        &mut session,
                        rewind_workspace.as_ref().expect("startup owner"),
                        &management_effects,
                        &args.cwd,
                        has_pending_inputs,
                        clipboard::copy_to_clipboard,
                    );
                    match result {
                        Ok(lines) => {
                            if matches!(operation, slash::Action::Clear)
                                && lines
                                    .iter()
                                    .any(|line| line.to_string().starts_with("T08 complete"))
                            {
                                local_lines.clear();
                                let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                conversation_view.reset(
                                    &render::HeaderInfo {
                                        provider_name: &provider_name,
                                        model_name: &model_name,
                                        usage: cumulative_usage,
                                        cwd: &cwd_display,
                                        cwd_short: &cwd_footer_display,
                                        effort_name: &effort_name,
                                    },
                                    width,
                                );
                            }
                            local_lines.extend(lines);
                            status = Status::Notice("Management operation result displayed".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(unix)]
                Some(slash::Action::RuntimeControl(request)) => {
                    if let Some(result) = handle_queued_control(
                        &request,
                        &terminal,
                        &mut key_reader,
                        &session,
                        rewind_workspace.as_ref().expect("startup owner"),
                        &management_effects,
                        &args.state_dir,
                        &mut queued_instructions,
                        &mut active_instruction,
                    ) {
                        match result {
                            Ok(lines) => {
                                local_lines
                                    .extend(lines.into_iter().map(ratatui::text::Line::from));
                                status = Status::Notice("Admitted input result displayed".into());
                            }
                            Err(error) => {
                                queued_inputs_paused = true;
                                status = Status::Error(error);
                            }
                        }
                        continue;
                    }

                    if request == "roles" || request.starts_with("roles ") {
                        match role_commands::handle(
                            &request,
                            &terminal,
                            &mut key_reader,
                            &session,
                            &initial_saved_settings,
                            rewind_workspace.as_ref().expect("startup owner"),
                            &management_effects,
                            args.provider.as_ref(),
                            &effective_agent_types,
                            has_pending_inputs,
                        ) {
                            Ok(lines) => {
                                local_lines.extend(lines);
                                refresh_saved_settings = true;
                                status = Status::Notice("Role binding result displayed".into());
                            }
                            Err(error) => status = Status::Error(error),
                        }
                        continue;
                    }
                    if request.starts_with("sandbox ") && request != "sandbox status" {
                        match sandbox_commands::handle(
                            &request,
                            &terminal,
                            &mut key_reader,
                            &session,
                            rewind_workspace.as_ref().expect("startup owner"),
                            &management_effects,
                            &args.sandbox,
                            has_pending_inputs,
                        ) {
                            Ok(lines) => {
                                local_lines.extend(lines);
                                status =
                                    Status::Notice("Sandbox execution result displayed".into());
                            }
                            Err(error) => status = Status::Error(error),
                        }
                        continue;
                    }
                    let result = handle_runtime_command(
                        &request,
                        session.delegation_control.as_ref(),
                        session.persistence.as_ref(),
                        &args.state_dir,
                        &effective_agent_types,
                        &args.sandbox,
                        approval_policy,
                        &mut next_max_turns,
                        args.max_turns,
                        &conversation_view,
                    )
                    .await;
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines.into_iter().map(ratatui::text::Line::from));
                            status = Status::Notice("Execution state displayed".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(unix)]
                Some(slash::Action::Account(_)) | Some(slash::Action::Logout) => {
                    let request = match parsed_action.as_ref().expect("matched action") {
                        slash::Action::Account(request) => request.as_str(),
                        _ => "logout",
                    };
                    let result = if request.is_empty() || request == "list" {
                        Ok(())
                    } else {
                        session.check_persistence().map_err(|e| e.to_string())
                    };
                    let result = match result {
                        Ok(()) => {
                            account::handle(
                                request,
                                &terminal,
                                &mut key_reader,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                args.account_binding.as_ref(),
                                has_pending_inputs,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            status =
                                Status::Notice("Authentication operation result displayed".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                Some(slash::Action::Status) => {
                    local_lines.extend(status_lines(StatusSnapshot {
                        session: &session,
                        session_path: Some(&session_path),
                        accounts: account_list_lines(),
                        #[cfg(unix)]
                        binding: args.account_binding.as_deref(),
                        provider_name: &provider_name,
                        model_name: &model_name,
                        usage_report: usage_meter.snapshot(),
                        sandbox: &args.sandbox,
                        approval: approval_policy,
                        startup_approval: args.approval_policy,
                        mcp_external: args.mcp_external,
                        audit_path: Some(&args.audit_path),
                        turn_authority_present: args.turn_authority.is_some(),
                        fixed_effort: args.initial_effort_name.as_deref(),
                        answer_finalizer: args.answer_finalizer.as_deref(),
                        #[cfg(unix)]
                        project_root: &polaris_core::project::resolve_root(&args.cwd),
                        #[cfg(unix)]
                        management_effects: &management_effects,
                        #[cfg(unix)]
                        recovery_issue: workflow_recovery_issue.as_deref(),
                    }));
                    local_lines.extend(
                        polaris_core::decision_diagnostics::display(
                            &args.decision_configuration,
                            args.decider.as_deref(),
                        )
                        .into_iter()
                        .map(ratatui::text::Line::from),
                    );
                    status = Status::Notice("Status displayed".into());
                    continue;
                }
                Some(slash::Action::Doctor) => {
                    local_lines.extend(doctor_lines(
                        &provider_name,
                        &model_name,
                        &args.cwd,
                        &args.sandbox,
                        approval_policy,
                        args.mcp_external,
                    ));
                    #[cfg(unix)]
                    local_lines.extend(management_effect_diagnostic(
                        &polaris_core::project::resolve_root(&args.cwd),
                        &management_effects,
                        workflow_recovery_issue.as_deref(),
                    ));
                    local_lines.extend(cat_diagnostic_lines(args.answer_finalizer.as_deref()));
                    #[cfg(unix)]
                    local_lines.extend(rewind_candidate_diagnostic(
                        rewind_workspace.as_ref(),
                        &args.sessions_dir,
                        &args.cwd,
                    ));
                    status = Status::Notice("Diagnostic results displayed".into());
                    continue;
                }
                Some(slash::Action::Mcp(requested))
                    if requested.split_whitespace().next() == Some("prompt") =>
                {
                    if requested == "prompt cancel" {
                        pending_mcp_prompt = None;
                        mcp_prompt_requires_reselect = false;
                        status = Status::Notice("MCP prompt selection cancelled".into());
                        continue;
                    }
                    match mcp_prompt_choice(&requested) {
                        Ok(choice) => {
                            status = Status::Notice(format!(
                                "MCP prompt {}/{} will be selected for the next input",
                                render::sanitize(&choice.server),
                                render::sanitize(&choice.prompt)
                            ));
                            pending_mcp_prompt = Some(choice);
                            mcp_prompt_requires_reselect = false;
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                Some(slash::Action::Review(extra)) => review_prompt(&extra),
                Some(slash::Action::Resume) => {
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    let previous_session_path = session_path.clone();
                    handle_resume(
                        &terminal,
                        &mut key_reader,
                        &args.sessions_dir,
                        &args.state_dir,
                        &cwd_display,
                        &args.workflow_config,
                        args.workflow_mode_overridden,
                        &effective_skills,
                        args.history_backend_factory.as_ref(),
                        #[cfg(unix)]
                        rewind_workspace.as_ref(),
                        #[cfg(unix)]
                        Some(&management_effects),
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                        &mut local_lines,
                    )
                    .await;
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    clear_mcp_prompt_on_session_change(
                        &previous_session_path,
                        &session_path,
                        &mut pending_mcp_prompt,
                        &mut mcp_prompt_requires_reselect,
                    );
                    // Same reasoning as the popup-selection path above: a
                    // resumed session's length alone can't be trusted to
                    // signal "this is a different conversation."
                    let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                    reset_conversation_view_after_resume(
                        &mut conversation_view,
                        &render::HeaderInfo {
                            provider_name: &provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        width,
                    );
                    continue;
                }
                Some(slash::Action::New) => {
                    let previous_session_path = session_path.clone();
                    #[cfg(unix)]
                    let switched = handle_managed_new_session(
                        &args.sessions_dir,
                        &args.state_dir,
                        rewind_workspace.as_ref().expect("Unix workspace owner"),
                        &management_effects,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                        &mut local_lines,
                    );
                    #[cfg(not(unix))]
                    let switched = handle_new_session(
                        &args.sessions_dir,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                        &mut local_lines,
                    );
                    if !switched {
                        restore_rejected_new_submission(
                            typed,
                            cursor_before_submit,
                            dispatching_queued_input,
                            &mut input_buffer,
                            &mut input_cursor,
                            &mut queued_inputs,
                            &mut queued_inputs_paused,
                        );
                        continue;
                    }
                    queued_inputs_paused = false;
                    record_submission(&mut input_history, &typed, dispatching_queued_input);
                    clear_mcp_prompt_on_session_change(
                        &previous_session_path,
                        &session_path,
                        &mut pending_mcp_prompt,
                        &mut mcp_prompt_requires_reselect,
                    );
                    let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                    conversation_view.reset(
                        &render::HeaderInfo {
                            provider_name: &provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        width,
                    );
                    continue;
                }
                Some(slash::Action::Fork) => {
                    let previous_session_path = session_path.clone();
                    #[cfg(unix)]
                    handle_managed_fork(
                        &args.sessions_dir,
                        &cwd_display,
                        &args.state_dir,
                        rewind_workspace.as_ref().expect("Unix workspace owner"),
                        &management_effects,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                    );
                    #[cfg(not(unix))]
                    handle_fork(
                        &args.sessions_dir,
                        &cwd_display,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                    );
                    clear_mcp_prompt_on_session_change(
                        &previous_session_path,
                        &session_path,
                        &mut pending_mcp_prompt,
                        &mut mcp_prompt_requires_reselect,
                    );
                    let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                    conversation_view.reset(
                        &render::HeaderInfo {
                            provider_name: &provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        width,
                    );
                    continue;
                }
                Some(slash::Action::Permissions) => {
                    handle_permissions(
                        &terminal,
                        &mut key_reader,
                        &mut approval_policy,
                        args.approval_policy,
                        &args.sandbox,
                        &mut status,
                    );
                    continue;
                }
                Some(slash::Action::Model) => {
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    handle_model(
                        &terminal,
                        &mut key_reader,
                        &mut args.provider,
                        &cloud_provider,
                        &usage_meter,
                        &local_router,
                        &mut provider_name,
                        &cloud_provider_name,
                        &mut model_name,
                        &cloud_effort_name,
                        &mut effort_name,
                        &mut status,
                    )
                    .await;
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    continue;
                }
                Some(slash::Action::Skills) => {
                    run_skills_picker(&terminal, &mut key_reader, &effective_skills);
                    status = Status::Idle;
                    continue;
                }
                #[cfg(unix)]
                Some(action @ (slash::Action::Phase(_) | slash::Action::Workflow(_))) => {
                    handle_managed_workflow_action(
                        &terminal,
                        &mut key_reader,
                        action,
                        &mut session,
                        &effective_skills,
                        rewind_workspace.as_ref().expect("Unix workspace owner"),
                        &management_effects,
                        &args.state_dir,
                        has_pending_inputs || dispatching_queued_input,
                        &mut status,
                    );
                    continue;
                }
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                Some(slash::Action::Sync(requested)) => {
                    if is_c10_sync_request(&requested) {
                        #[cfg(target_os = "macos")]
                        match session
                            .check_persistence()
                            .map_err(|error| error.to_string())
                            .and_then(|()| {
                                start_c10_sync_action(
                                    &args.cwd,
                                    &args.state_dir,
                                    &requested,
                                    has_pending_inputs || dispatching_queued_input,
                                )
                            }) {
                            Ok(job) => {
                                pending_c10_job = Some(job);
                                status = Status::Notice("C10 operation started".into());
                            }
                            Err(error) => {
                                record_c10_result(Err(error), &mut status, &mut local_lines)
                            }
                        }
                        #[cfg(target_os = "linux")]
                        record_c10_result(
                            c10_unavailable_action(
                                &requested,
                                has_pending_inputs || dispatching_queued_input,
                            ),
                            &mut status,
                            &mut local_lines,
                        );
                        continue;
                    }
                    handle_sync_action(
                        &terminal,
                        &mut key_reader,
                        &args.cwd,
                        &args.state_dir,
                        &requested,
                        has_pending_inputs || dispatching_queued_input,
                        &session,
                        &mut status,
                        &mut local_lines,
                    );
                    continue;
                }
                #[cfg(target_os = "linux")]
                Some(slash::Action::Apply(requested)) => {
                    handle_source_apply_action(
                        &terminal,
                        &mut key_reader,
                        &args,
                        audit.clone(),
                        &mut source_frontend,
                        &requested,
                        has_pending_inputs || dispatching_queued_input,
                        &session,
                        &mut status,
                        &mut local_lines,
                    )
                    .await;
                    continue;
                }
                Some(slash::Action::Compact) => {
                    // Same reasoning as the popup-selection path above:
                    // show feedback before the (potentially tens-of-
                    // seconds-long) summarization call rather than
                    // leaving the last drawn frame on screen.
                    status = Status::Notice("compacting...".to_string());
                    if draw_chat_frame(
                        &mut terminal.borrow_mut(),
                        &input_buffer,
                        input_cursor,
                        0,
                        |f| {
                            draw_frame(
                                f,
                                &conversation_view.history,
                                &mut conversation_view.history_wrap_cache,
                                conversation_view.scroll_offset,
                                &input_buffer,
                                input_cursor,
                                &status,
                                &render::HeaderInfo {
                                    provider_name: &provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                &[],
                                0,
                                &conversation_view.selection,
                            )
                        },
                    )
                    .is_err()
                    {
                        break ExitCode::FAILURE;
                    }
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        break 'outer code;
                    }
                    #[cfg(unix)]
                    let _compact_persistence_failed = match local_actions::compact(
                        &terminal,
                        &mut key_reader,
                        args.provider.as_ref(),
                        &mut session,
                        &session_path,
                        rewind_workspace.as_ref().expect("startup owner"),
                        &management_effects,
                        &mut status,
                        &mut cumulative_usage,
                        has_pending_inputs,
                    )
                    .await
                    {
                        Ok(_) => false,
                        Err(error) => {
                            status = Status::Error(error);
                            true
                        }
                    };
                    #[cfg(not(unix))]
                    let _compact_persistence_failed = handle_compact(
                        args.provider.as_ref(),
                        &mut session,
                        &session_path,
                        &mut status,
                        &mut cumulative_usage,
                    )
                    .await;
                    #[cfg(target_os = "linux")]
                    if let Some(code) = linux_pending_exit_code() {
                        if _compact_persistence_failed {
                            fatal_message = Some(
                                "compacted history could not be persisted during shutdown".into(),
                            );
                            break 'outer ExitCode::FAILURE;
                        }
                        break 'outer code;
                    }
                    conversation_view.mark_messages_printed(session.messages.len());
                    continue;
                }
                Some(action) => {
                    // Same reasoning as the popup-selection path above:
                    // `apply_slash_action` can't re-seed `history`'s header
                    // or reset `scroll_offset` itself for `Clear`.
                    let clear_view = matches!(action, slash::Action::Clear);
                    match apply_slash_action(
                        action,
                        &mut session,
                        &session_path,
                        &mut status,
                        &provider_name,
                        &model_name,
                        usage_meter.snapshot(),
                        &effective_skills,
                        &args.cwd,
                        &args.state_dir,
                        &args.sessions_dir,
                        args.mcp_external,
                        &mut local_lines,
                    ) {
                        SlashOutcome::Continue => {
                            if clear_view {
                                let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                conversation_view.reset(
                                    &render::HeaderInfo {
                                        provider_name: &provider_name,
                                        model_name: &model_name,
                                        usage: cumulative_usage,
                                        cwd: &cwd_display,
                                        cwd_short: &cwd_footer_display,
                                        effort_name: &effort_name,
                                    },
                                    width,
                                );
                            }
                            continue;
                        }
                        SlashOutcome::Quit => {
                            #[cfg(unix)]
                            if !queued_instructions.is_empty() || active_instruction.is_some() {
                                status = Status::Notice(
                                    "Process or cancel admitted instructions before exiting".into(),
                                );
                                continue;
                            }
                            if refuse_quit_with_pending_work(
                                &queued_inputs,
                                &mut input_history,
                                (&mut input_buffer, &mut input_cursor),
                                None,
                                &typed,
                                dispatching_queued_input,
                                &mut status,
                            ) {
                                continue;
                            }
                            break ExitCode::SUCCESS;
                        }
                        SlashOutcome::Fatal(msg) => {
                            fatal_message = Some(msg);
                            break 'outer ExitCode::FAILURE;
                        }
                        #[cfg(target_os = "linux")]
                        SlashOutcome::Shutdown(reason) => {
                            break 'outer linux_signal_exit_code(reason);
                        }
                    }
                }
                None => typed,
            }
        };

        if let Err(error) = require_turn_owner() {
            restore_rejected_new_submission(
                text,
                cursor_before_submit,
                dispatching_queued_input,
                &mut input_buffer,
                &mut input_cursor,
                &mut queued_inputs,
                &mut queued_inputs_paused,
            );
            status = Status::Error(error.into());
            continue;
        }

        // Admission can save drafts, capture a rewind boundary, and await
        // sidechat routing.  Echo the submitted text before that work starts
        // while keeping the preview out of the durable session until admission
        // succeeds.
        let preview_suggestions = slash_suggestions(&input_buffer, &input_history, false);
        let preview_selection = conversation_view.selection.clone();
        if draw_submission_preview(
            &mut conversation_view,
            &mut status,
            &text,
            |preview_history, preview_wrap_cache, preview_status| {
                draw_chat_frame(
                    &mut terminal.borrow_mut(),
                    &input_buffer,
                    input_cursor,
                    preview_suggestions.len(),
                    |f| {
                        draw_frame(
                            f,
                            preview_history,
                            preview_wrap_cache,
                            0,
                            &input_buffer,
                            input_cursor,
                            preview_status,
                            &render::HeaderInfo {
                                provider_name: &provider_name,
                                model_name: &model_name,
                                usage: cumulative_usage,
                                cwd: &cwd_display,
                                cwd_short: &cwd_footer_display,
                                effort_name: &effort_name,
                            },
                            &preview_suggestions,
                            selected_suggestion,
                            &preview_selection,
                        )
                    },
                )
                .map(|_| ())
            },
        )
        .is_err()
        {
            break 'outer ExitCode::FAILURE;
        }

        #[cfg(unix)]
        if session.persistence.is_some() && !dispatching_queued_input {
            if let Err(error) = draft::save(
                &session,
                &initial_saved_settings,
                rewind_workspace.as_ref().expect("startup owner"),
                &management_effects,
                "",
            ) {
                input_buffer = text;
                input_cursor = input_buffer.len();
                status = Status::Error(format!("Cannot save draft send preparation: {error}"));
                continue;
            }
            draft_saved_text.clear();
            draft_candidate.clear();
        }
        #[cfg(unix)]
        let rewind_pre_manifest_id = if session.persistence.is_some() {
            match rewind_workspace.as_ref() {
                Some(owner) => {
                    match capture_rewind_before_input(owner, &management_effects, &args.sandbox) {
                        Ok(id) => Some(id.to_string()),
                        Err(error) => {
                            rewind_unavailable_reason =
                                format!("Cannot create a file snapshot for this input: {error}");
                            None
                        }
                    }
                }
                None => {
                    rewind_unavailable_reason =
                        "This workspace is not eligible for file rewind".to_owned();
                    None
                }
            }
        } else {
            rewind_unavailable_reason = "This conversation is not a saved v2 session".to_owned();
            None
        };
        #[cfg(not(unix))]
        let rewind_pre_manifest_id: Option<String> = None;

        #[cfg(unix)]
        let _project_activity = {
            let acquired = match rewind_workspace.as_ref() {
                Some(owner) => owner
                    .try_activity_quiescent()
                    .map_err(|error| error.to_string()),
                None => {
                    polaris_core::project_mutation::ProjectMutationOwnerV1::for_project(&args.cwd)
                        .and_then(|owner| owner.try_activity())
                        .map_err(|error| error.to_string())
                }
            };
            match acquired {
                Ok(lease) => std::sync::Arc::new(lease),
                Err(error) => {
                    if dispatching_queued_input {
                        queued_inputs.push_front(text);
                        queued_inputs_paused = true;
                    } else {
                        input_buffer = text;
                        input_cursor = input_buffer.len();
                    }
                    status = Status::Error(format!(
                        "Cannot start this input because the workspace is in use: {error}"
                    ));
                    continue;
                }
            }
        };

        #[cfg(unix)]
        match polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_activity(
            &polaris_core::project::resolve_root(&args.cwd),
            &args.state_dir,
            &_project_activity,
        ) {
            Ok(Some(effect)) => {
                if dispatching_queued_input {
                    queued_inputs.push_front(text);
                    queued_inputs_paused = true;
                } else {
                    input_buffer = text;
                    input_cursor = input_buffer.len();
                }
                status = Status::Error(format!(
                    "Unresolved management operation {} ({}). New input cannot start before recovery",
                    effect.request.operation_id, effect.request.inventory
                ));
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                if dispatching_queued_input {
                    queued_inputs.push_front(text);
                    queued_inputs_paused = true;
                } else {
                    input_buffer = text;
                    input_cursor = input_buffer.len();
                }
                status =
                    Status::Error(format!("Cannot verify management operation state: {error}"));
                continue;
            }
        }

        #[cfg(unix)]
        if let Some(persistence) = session.persistence.as_ref() {
            let admission = rewind_workspace
                .as_ref()
                .ok_or_else(|| "No workspace file verification owner".to_string())
                .and_then(|owner| {
                    let branch =
                        polaris_core::rewind_admission::published_branch(owner, &persistence.store)
                            .map_err(|error| error.to_string())?;
                    if branch.is_some() {
                        validate_resume_workspace(owner, persistence)?;
                    }
                    Ok::<_, String>(())
                });
            if let Err(error) = admission {
                if dispatching_queued_input {
                    queued_inputs.push_front(text);
                    queued_inputs_paused = true;
                } else {
                    input_buffer = text;
                    input_cursor = input_buffer.len();
                }
                status = Status::Error(format!(
                    "Cannot verify conversation execution permission: {error}"
                ));
                continue;
            }
        }

        #[cfg(unix)]
        if let (Some(owner), Some(id)) = (rewind_workspace.as_ref(), &rewind_pre_manifest_id) {
            let matches = id
                .parse()
                .map_err(|error| format!("Invalid saved worktree ID: {error}"))
                .and_then(|manifest| {
                    owner
                        .matches_manifest(&manifest)
                        .map_err(|error| format!("Cannot reconcile the worktree: {error}"))
                });
            if !matches.is_ok_and(|matches| matches) {
                if dispatching_queued_input {
                    queued_inputs.push_front(text);
                    queued_inputs_paused = true;
                } else {
                    input_buffer = text;
                    input_cursor = input_buffer.len();
                }
                status = Status::Error(
                    "Workspace files changed during input preparation. Inspect state before resending"
                        .into(),
                );
                continue;
            }
        }

        if mcp_prompt_requires_reselect {
            if dispatching_queued_input {
                queued_inputs.push_front(text);
                queued_inputs_paused = true;
            } else {
                input_buffer = text;
                input_cursor = input_buffer.len();
            }
            status = Status::Error(
                "MCP prompt selection outcome is unknown. Reselect with /mcp prompt or cancel with /mcp prompt cancel"
                    .into(),
            );
            continue;
        }
        if pending_mcp_prompt.is_some() && (session.persistence.is_none() || mcp_host.is_none()) {
            if dispatching_queued_input {
                queued_inputs.push_front(text);
                queued_inputs_paused = true;
            } else {
                input_buffer = text;
                input_cursor = input_buffer.len();
            }
            status = Status::Error(if session.persistence.is_none() {
                "MCP prompts require a v2 conversation. Migrate the conversation before selecting"
                    .into()
            } else {
                "MCP servers are unavailable. Input was not started".into()
            });
            continue;
        }

        #[cfg(target_os = "linux")]
        if let Some(code) = linux_pending_exit_code() {
            break 'outer code;
        }

        session.automatic_effort =
            args.initial_effort_name.is_none() && !is_local_provider(&provider_name);
        if session.automatic_effort {
            effort_name = UNDECIDED_EFFORT.to_string();
        }
        let mut tui_admission = None;
        let mut selected_mcp_runtime = None;
        let mut rewind_boundary: Option<polaris_core::conversation_state::RewindBoundaryV1> = None;
        #[cfg(unix)]
        if let Some(instruction) = active_instruction.take() {
            let prepared = (|| -> Result<TuiAdmissionTurn, String> {
                session
                    .persistence
                    .as_ref()
                    .ok_or("No conversation for the saved instruction")?;
                let (store, mut manager) =
                    open_tui_admission_owner(&args.state_dir, instruction.parent_session())?;
                instruction_commands::preflight(&manager, &session, &instruction)?;
                if let Some(installer) = args.turn_authority.as_ref() {
                    installer(&mut session, &text)?;
                }
                rewind_boundary = begin_rewind_for_session(
                    &session,
                    rewind_pre_manifest_id.as_deref(),
                    &args.cwd,
                    &args.state_dir,
                    memory_activation.tool_memory_enabled(),
                )
                .map_err(|error| error.to_string())?;
                let applied =
                    instruction_commands::apply(&store, &mut manager, &mut session, instruction)?;
                let current = session
                    .persistence
                    .as_ref()
                    .ok_or("No conversation for the saved instruction")?
                    .snapshot()
                    .map_err(|e| e.to_string())?
                    .state
                    .session_id;
                if applied.session_id.as_str() != current
                    || applied.run_id
                        != polaris_core::session_manager::RunId::derive(&applied.admission_id)
                {
                    return Err(
                        "Saved instruction destination and admission record mismatch".into(),
                    );
                }
                Ok(TuiAdmissionTurn {
                    store,
                    manager,
                    admission_id: applied.admission_id,
                })
            })();
            match prepared {
                Ok(admission) => tui_admission = Some(admission),
                Err(error) => {
                    fatal_message = Some(format!(
                        "Cannot verify instruction application. Do not resend; recover first: {error}"
                    ));
                    break 'outer ExitCode::FAILURE;
                }
            }
        }
        if session.persistence.is_some() && tui_admission.is_none() {
            let prompt_request =
                pending_mcp_prompt
                    .as_ref()
                    .zip(mcp_host.as_ref())
                    .map(|(choice, host)| TuiPromptRequest {
                        host,
                        choice,
                        selector: args.mcp_selection_provider.clone(),
                        ceiling: mcp_ceiling,
                    });
            match admit_tui_input(
                &mut session,
                &text,
                args.decider.as_deref(),
                &args.state_dir,
                &args.sessions_dir,
                &mut session_path,
                &mut meta_path,
                &mut session_started_at_millis,
                &mut local_lines,
                args.turn_authority.as_ref(),
                rewind_pre_manifest_id.as_deref(),
                args.tool_memory.is_some(),
                &args.cwd,
                args.sidechat_auto,
                prompt_request,
            )
            .await
            {
                Ok((admission, notice, boundary, runtime)) => {
                    tui_admission = Some(admission);
                    rewind_boundary = boundary;
                    selected_mcp_runtime = runtime;
                    if selected_mcp_runtime.is_some()
                        && let Some(choice) = pending_mcp_prompt.take()
                    {
                        local_lines.push(ratatui::text::Line::from(format!(
                            "Selected MCP prompt {}/{}",
                            render::sanitize(&choice.server),
                            render::sanitize(&choice.prompt)
                        )));
                    }
                    if let Some(notice) = notice {
                        local_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                            notice,
                            ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
                        )));
                        tool_memory_path = None;
                    }
                }
                Err(TuiAdmissionFailure::Retryable(error)) => {
                    input_buffer = text;
                    input_cursor = input_buffer.len();
                    status = Status::Error(format!("Cannot route input: {error}"));
                    continue;
                }
                Err(TuiAdmissionFailure::PromptFailed { message, unknown }) => {
                    pending_mcp_prompt = None;
                    mcp_prompt_requires_reselect = true;
                    if dispatching_queued_input {
                        queued_inputs.push_front(text);
                        queued_inputs_paused = true;
                    } else {
                        input_buffer = text;
                        input_cursor = input_buffer.len();
                    }
                    status = Status::Error(format!(
                        "{message}. Not added to conversation history. {}. Reselect with /mcp prompt",
                        if unknown {
                            "Retrieval outcome is unknown"
                        } else {
                            "Model execution was not started"
                        }
                    ));
                    continue;
                }
                Err(error @ TuiAdmissionFailure::Consumed(_)) => {
                    fatal_message = Some(format!(
                        "Cannot verify input storage or execution start: {}. Check recovery state after restarting.",
                        error.message()
                    ));
                    break 'outer ExitCode::FAILURE;
                }
            }
        } else if session.persistence.is_none() {
            if let Some(installer) = &args.turn_authority
                && let Err(error) = installer(&mut session, &text)
            {
                input_buffer = text;
                input_cursor = input_buffer.len();
                status = Status::Error(format!("Cannot attach routine authority: {error}"));
                continue;
            }
            #[cfg(unix)]
            match begin_rewind_for_session(
                &session,
                rewind_pre_manifest_id.as_deref(),
                &args.cwd,
                &args.state_dir,
                args.tool_memory.is_some(),
            ) {
                Ok(boundary) => rewind_boundary = boundary,
                Err(error) => {
                    rewind_unavailable_reason =
                        format!("Cannot save the conversation boundary for this input: {error}");
                }
            }
            session.push_user(&text);
        }
        // A distinct input can replace `session` with an empty sidechat
        // after the loop-top attachment. Reinstall only process-owned
        // capabilities; conversation history, approval replies, tool state,
        // and memory are deliberately not copied from the parent.
        attach_process_services(
            &mut session,
            ProcessServices {
                decider: &args.decider,
                tier_decider: &args.tier_decider,
                light_model: &args.mcp_selection_provider,
                approval_auto: args.approval_auto,
                setting: &setting,
                computer: &computer,
            },
        );
        // `mut`: mid-turn compaction (see `history_compacted_this_turn`
        // below) can shrink `session.messages` after this point but within
        // the same turn, which would otherwise leave `checkpoint` pointing
        // past the unbalanced tool-call tail `TurnOutcome::Done(Err(_))`/
        // `Interrupted` roll back to — `Vec::truncate` silently no-ops
        // when given a length >= the vec's current length, so a stale
        // `checkpoint` would make that rollback quietly do nothing.
        let mut checkpoint = session.messages.len();
        if session.persistence.is_none() {
            // Legacy sessions are created lazily. v2 already has an atomic
            // marker and `Session::push_user` has appended the raw event.
            if let Err(e) = persist::write_meta_if_absent(
                &meta_path,
                &persist::SessionMeta {
                    cwd: cwd_display.clone(),
                    started_at_millis: session_started_at_millis,
                },
            ) {
                let message = format!("Can't write the session metadata: {e}");
                #[cfg(target_os = "linux")]
                let message = match linux_finish_pre_turn_shutdown(
                    tui_admission.take(),
                    selected_mcp_runtime.take(),
                    rewind_boundary.take(),
                    session.persistence.as_ref(),
                )
                .await
                {
                    Ok(()) => message,
                    Err(cleanup) => format!("{message}; {cleanup}"),
                };
                fatal_message = Some(message);
                break 'outer ExitCode::FAILURE;
            }
            if let Err(e) = persist::append_message(
                &session_path,
                session.messages.last().expect("just pushed"),
            ) {
                let message = format!("Can't persist the message: {e}");
                #[cfg(target_os = "linux")]
                let message = match linux_finish_pre_turn_shutdown(
                    tui_admission.take(),
                    selected_mcp_runtime.take(),
                    rewind_boundary.take(),
                    session.persistence.as_ref(),
                )
                .await
                {
                    Ok(()) => message,
                    Err(cleanup) => format!("{message}; {cleanup}"),
                };
                fatal_message = Some(message);
                break 'outer ExitCode::FAILURE;
            }
        }
        if let Err(e) = session.check_persistence() {
            let message = format!("Can't persist the message: {e}");
            #[cfg(target_os = "linux")]
            let message = match linux_finish_pre_turn_shutdown(
                tui_admission.take(),
                selected_mcp_runtime.take(),
                rewind_boundary.take(),
                session.persistence.as_ref(),
            )
            .await
            {
                Ok(()) => message,
                Err(cleanup) => format!("{message}; {cleanup}"),
            };
            fatal_message = Some(message);
            break 'outer ExitCode::FAILURE;
        }

        #[cfg(target_os = "linux")]
        if let Some(reason) = signal::pending_linux_process_shutdown() {
            if let Err(error) = linux_finish_pre_turn_shutdown(
                tui_admission.take(),
                selected_mcp_runtime.take(),
                rewind_boundary.take(),
                session.persistence.as_ref(),
            )
            .await
            {
                fatal_message = Some(error);
                break 'outer ExitCode::FAILURE;
            }
            break 'outer linux_signal_exit_code(reason);
        }

        // Prompt selection, when requested, already committed its result
        // atomically with the input before the run was started.
        mcp_turns += 1;
        let durable_session_id = session
            .persistence
            .as_ref()
            .and_then(|store| store.snapshot().ok())
            .map(|snapshot| snapshot.state.session_id)
            .or_else(|| {
                session_path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("tui-session-{}", std::process::id()));
        let mcp_run_id = match tui_admission.as_ref() {
            Some(admission) => match admission.run_id() {
                Ok(run_id) => run_id.to_owned(),
                Err(error) => {
                    let message = format!("Cannot verify the sidechat run ID: {error}");
                    #[cfg(target_os = "linux")]
                    let message = match linux_finish_pre_turn_shutdown(
                        tui_admission.take(),
                        selected_mcp_runtime.take(),
                        rewind_boundary.take(),
                        session.persistence.as_ref(),
                    )
                    .await
                    {
                        Ok(()) => message,
                        Err(cleanup) => format!("{message}; {cleanup}"),
                    };
                    fatal_message = Some(message);
                    break 'outer ExitCode::FAILURE;
                }
            },
            None => format!("tui-{}-{}-{mcp_turns}", std::process::id(), now_millis()),
        };
        let mut mcp_runtime = selected_mcp_runtime.or_else(|| {
            mcp_host.as_ref().and_then(|host| {
                host.runtime(&durable_session_id, &mcp_run_id, mcp_ceiling)
                    .ok()
                    .flatten()
                    .map(|(mut runtime, _)| {
                        host.configure_preselection(
                            &mut runtime,
                            args.mcp_selection_provider.clone(),
                            None,
                        );
                        runtime
                    })
            })
        });

        #[cfg(target_os = "linux")]
        if let Some(reason) = signal::pending_linux_process_shutdown() {
            if let Err(error) = linux_finish_pre_turn_shutdown(
                tui_admission.take(),
                mcp_runtime.take(),
                rewind_boundary.take(),
                session.persistence.as_ref(),
            )
            .await
            {
                fatal_message = Some(error);
                break 'outer ExitCode::FAILURE;
            }
            break 'outer linux_signal_exit_code(reason);
        }

        let turn_started = Instant::now();
        #[cfg(unix)]
        let mut rewind_effects = rewind_ui::EffectTracker::default();
        status = Status::Thinking {
            elapsed: Duration::ZERO,
        };
        // Print the user's own message immediately — otherwise it wouldn't
        // appear until the *next* top-of-loop print, which only happens
        // after the assistant's reply finishes too, making the user's own
        // input look laggy instead of showing up the instant it's sent.
        conversation_view.append_new_history(&session, &local_lines);
        let suggestions = slash_suggestions(&input_buffer, &input_history, false);
        if selected_suggestion >= suggestions.len() {
            selected_suggestion = 0;
        }
        let mut rendered_suggestion_count = suggestions.len();
        if draw_chat_frame(
            &mut terminal.borrow_mut(),
            &input_buffer,
            input_cursor,
            suggestions.len(),
            |f| {
                draw_frame(
                    f,
                    &conversation_view.history,
                    &mut conversation_view.history_wrap_cache,
                    conversation_view.scroll_offset,
                    &input_buffer,
                    input_cursor,
                    &status,
                    &render::HeaderInfo {
                        provider_name: &provider_name,
                        model_name: &model_name,
                        usage: cumulative_usage,
                        cwd: &cwd_display,
                        cwd_short: &cwd_footer_display,
                        effort_name: &effort_name,
                    },
                    &suggestions,
                    selected_suggestion,
                    &conversation_view.selection,
                )
            },
        )
        .is_err()
        {
            #[cfg(target_os = "linux")]
            if let Err(cleanup) = linux_finish_pre_turn_shutdown(
                tui_admission.take(),
                mcp_runtime.take(),
                rewind_boundary.take(),
                session.persistence.as_ref(),
            )
            .await
            {
                fatal_message = Some(format!("Cannot update the TUI display; {cleanup}"));
            }
            break ExitCode::FAILURE;
        }

        enum TurnOutcome {
            Done(Result<agent::AgentOutcome, agent::AgentError>),
            Interrupted,
            Fatal,
            #[cfg(target_os = "linux")]
            Shutdown(signal::LinuxProcessShutdown),
        }

        // Scoped so every borrow taken to build `agent_future` (of
        // `session`, `audit`, ...) ends when this block does — the match
        // below needs `&mut session` again, which it can't have while
        // any of these are still alive.
        session.mcp_setup = mcp_setup.clone();
        #[cfg(target_os = "linux")]
        if let Some(reason) = signal::pending_linux_process_shutdown() {
            if let Err(error) = linux_finish_pre_turn_shutdown(
                tui_admission.take(),
                mcp_runtime.take(),
                rewind_boundary.take(),
                session.persistence.as_ref(),
            )
            .await
            {
                fatal_message = Some(error);
                break 'outer ExitCode::FAILURE;
            }
            break 'outer linux_signal_exit_code(reason);
        }
        let turn_control = match install_tui_root_control(
            &mut session,
            TuiRootControlArgs {
                session_id: &durable_session_id,
                turn: mcp_turns,
                sandbox: &args.sandbox,
                approval: approval_policy,
                mcp_available: mcp_runtime.is_some(),
                tool_names: &args.tool_names,
                model: &model_name,
                effort: args.initial_effort_name.as_deref().unwrap_or("automatic"),
            },
        ) {
            Ok(control) => control,
            Err(error) => {
                #[cfg(target_os = "linux")]
                if let Err(cleanup) = linux_finish_pre_turn_shutdown(
                    tui_admission.take(),
                    mcp_runtime.take(),
                    rewind_boundary.take(),
                    session.persistence.as_ref(),
                )
                .await
                {
                    fatal_message = Some(format!(
                        "Cannot prepare subagent control: {error}; {cleanup}"
                    ));
                    break 'outer ExitCode::FAILURE;
                }
                #[cfg(not(target_os = "linux"))]
                if let (Some(boundary), Some(persistence)) =
                    (rewind_boundary.take(), session.persistence.as_ref())
                {
                    let _ = persistence.abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "Cannot start subagent control".into(),
                    );
                }
                status = Status::Error(format!("Cannot prepare subagent control: {error}"));
                #[cfg(target_os = "linux")]
                if let Some(code) = linux_pending_exit_code() {
                    break 'outer code;
                }
                continue 'outer;
            }
        };
        #[cfg(unix)]
        let runtime_generation = match session
            .persistence
            .as_ref()
            .map(|p| p.snapshot())
            .transpose()
        {
            Ok(Some(snapshot)) => snapshot.state.generation,
            Ok(None) => 0,
            Err(error) => {
                status = Status::Error(format!(
                    "Cannot read saved execution management state: {error}"
                ));
                continue 'outer;
            }
        };
        #[cfg(unix)]
        let runtime_error = RefCell::new(None);
        #[cfg(unix)]
        let mut interruption_effect = None;
        if let Some(active) = tui_admission.as_ref() {
            conversation_view.push_history_line(render::HistoryLine {
                line: ratatui::text::Line::from(format!(
                    "Admission ID: {} (/input status|cancel)",
                    active.admission_id.as_str()
                )),
                shaded: true,
            });
        }
        #[cfg(unix)]
        let runtime_persistence = session.persistence.clone();
        #[cfg(unix)]
        let child_provider = match role_commands::activation(
            args.provider.clone(),
            &effective_saved_settings,
            &effective_agent_types,
        ) {
            Ok(provider) => provider,
            Err(error) => {
                fatal_message = Some(format!("Cannot attach role bindings to the run: {error}"));
                break 'outer ExitCode::FAILURE;
            }
        };
        #[cfg(not(unix))]
        let child_provider = args.provider.clone();
        #[cfg(target_os = "macos")]
        let _environment_turn = {
            let tool = std::sync::Arc::new(polaris_core::environment::tool::EnvironmentTool::new(
                polaris_core::project::resolve_root(&args.cwd),
                args.cwd.clone(),
            ));
            let binding = match tool.bind(
                management_effects.clone(),
                std::sync::Arc::clone(&_project_activity),
                &durable_session_id,
            ) {
                Ok(binding) => binding,
                Err(error) => {
                    fatal_message = Some(error);
                    break 'outer ExitCode::FAILURE;
                }
            };
            session.environment = Some(tool);
            binding
        };
        let outcome = {
            let mut stop = StopTracker::new(next_max_turns);
            let mut gate = Gate::new(approval_policy);
            let approver = TuiApprover {
                terminal: &terminal,
                reader: &mut key_reader,
            };
            #[cfg(unix)]
            let mut approver = runtime_effects::JournalledApprover {
                inner: approver,
                context: runtime_commands::RuntimeMutationContext {
                    effects: &management_effects,
                    lease: &_project_activity,
                    session_id: &durable_session_id,
                    generation: runtime_generation,
                    accepted: false,
                },
                error: &runtime_error,
            };
            #[cfg(not(unix))]
            let mut approver = approver;
            let mut ctx = ToolContext {
                sandbox: &args.sandbox,
                helper: &args.helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            // Races `agent::run` against a redraw tick (so the status
            // row's elapsed-time/shimmer animates while the turn is in
            // flight) and the event stream (so Esc can interrupt it).
            // `agent_future` is only ever polled here, in one place, so
            // dropping it on interrupt is real cancellation: Rust futures
            // stop making progress the moment nothing polls them again,
            // which is what "esc to interrupt" means in practice — this
            // doesn't kill an already-spawned OS subprocess a tool call
            // may have started, only stops *waiting* on the turn.
            // The tick-redraw branch below only redraws the footer
            // (status row's elapsed-time/shimmer) — it never needs to read
            // `session` at all now that history is appended once via
            // `append_new_history` rather than redrawn from it every frame,
            // so no snapshot is needed to sidestep `agent_future`'s
            // mutable borrow of `session` below. Mid-turn tool activity
            // reaches the screen through `events_rx` instead, which
            // carries owned `AgentEvent`s and so borrows nothing.
            //
            // `Ctrl+O` (copy) is the one mid-turn key handler that does
            // need reply text, so it can't read `session` live once
            // `agent_future` below borrows it mutably — captured here,
            // before that borrow starts, as the reply from *before* this
            // turn (there can't be a newer one yet; the turn hasn't
            // produced a reply until it finishes).
            #[cfg(not(unix))]
            let reply_before_this_turn =
                clipboard::last_agent_message_text(&session).map(str::to_string);
            //
            // Unbounded on purpose: a bounded sender would make
            // `agent::run` block on a full queue, i.e. let the display
            // throttle the actual turn. Events are small and a turn's
            // worth of them is bounded by the turn itself.
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            let agent_future = agent::run_with_mcp(
                args.provider.as_ref(),
                &mut session,
                audit.clone(),
                &mut stop,
                args.always_on,
                &effective_skills,
                &effective_agent_types,
                child_provider,
                args.spawn_concurrency,
                args.spawn_write_concurrency,
                Some(events_tx),
                &mut ctx,
                mcp_runtime.as_mut(),
            );
            tokio::pin!(agent_future);
            let mut file_input_changed_at = Instant::now();
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it since the
            // pre-draw above already painted the elapsed=0 frame.
            ticker.tick().await;
            #[cfg(target_os = "linux")]
            let mut shutdown_tick = tokio::time::interval(LINUX_SIGNAL_POLL_INTERVAL);
            #[cfg(target_os = "linux")]
            shutdown_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            #[cfg(target_os = "linux")]
            shutdown_tick.tick().await;
            let mut event_stream = ratatui::crossterm::event::EventStream::new();
            // How long the `Ctrl+O` copy confirmation stays on screen
            // before the ticker branch resumes overwriting `status` with
            // the `Thinking` animation — without this, the confirmation
            // would only ever survive until the very next tick (at most
            // 100ms later), which reads as no feedback at all.
            let mut mid_turn_notice_until: Option<Instant> = None;

            let turn_outcome = loop {
                // A queued run may have restored a draft. These keys are
                // interactive even when that run began from queued input.
                let suggestions = slash_suggestions(&input_buffer, &input_history, false);
                if selected_suggestion >= suggestions.len() {
                    selected_suggestion = 0;
                }
                #[cfg(target_os = "linux")]
                if let Some(reason) = signal::pending_linux_process_shutdown() {
                    break TurnOutcome::Shutdown(reason);
                }
                tokio::select! {
                    biased;
                    _ = async {
                        #[cfg(target_os = "linux")]
                        { shutdown_tick.tick().await; }
                        #[cfg(not(target_os = "linux"))]
                        { std::future::pending::<()>().await; }
                    } => {
                        #[cfg(target_os = "linux")]
                        if let Some(reason) = signal::pending_linux_process_shutdown() {
                            break TurnOutcome::Shutdown(reason);
                        }
                    }
                    result = &mut agent_future => {
                        #[cfg(target_os = "linux")]
                        if let Some(reason) = signal::pending_linux_process_shutdown() {
                            break TurnOutcome::Shutdown(reason);
                        }
                        break TurnOutcome::Done(result);
                    }
                    // Ahead of the tick and the key-event arms so a burst
                    // of tool activity is printed as it happens rather
                    // than queueing behind cosmetic redraws; behind
                    // `agent_future` so the turn's own completion (and
                    // the rollback it may need) is never delayed by
                    // display work. Anything still queued when the turn
                    // finishes is drained right after this loop.
                    Some(event) = events_rx.recv() => {
                        #[cfg(unix)]
                        rewind_effects.observe(&event, |path| {
                            rewind_workspace.as_ref().is_some_and(|owner| {
                                owner.is_restorable_path(path).unwrap_or(false)
                            })
                        });
                        update_effort_from_event(&event, &mut effort_name);
                        conversation_view.append_live_event(&event);
                        if let polaris_core::AgentEvent::HistoryCompacted {
                            messages_before,
                            messages_after,
                            ..
                        } = event
                        {
                            history_compacted_this_turn = true;
                            // `checkpoint` was captured before this turn's
                            // compaction could have run — shift it back by
                            // however many messages compaction just
                            // removed so it still points at the same
                            // logical position (see the comment on
                            // `checkpoint`'s declaration above).
                            checkpoint = checkpoint
                                .saturating_sub(messages_before.saturating_sub(messages_after));
                            // Same staleness problem as `checkpoint`:
                            // `printed_messages` was also captured before
                            // this turn's compaction could have run. Left
                            // unadjusted, `append_new_history`'s
                            // shrink-guard (see its own doc) would fire the
                            // next time it's called whenever this turn
                            // removes more messages than it goes on to add,
                            // wiping the entire `history` buffer — header
                            // box included — instead of just rendering the
                            // turn's new messages.
                            conversation_view.observe_compaction(messages_before, messages_after);
                        }
                    }
                    _ = ticker.tick() => {
                        if file_input_changed_at.elapsed() >= Duration::from_millis(40)
                            && !is_secret_command(&input_buffer)
                        {
                            file_input::normalize_tail(&mut input_buffer, &mut input_cursor);
                        }
                        #[cfg(unix)]
                        if runtime_error.borrow().is_some(){break TurnOutcome::Fatal;}
                        if mid_turn_notice_until.is_some_and(|until| Instant::now() < until) {
                            // Leave `status` as the Ctrl+O confirmation —
                            // redrawing it unchanged below keeps it
                            // visible instead of the `Thinking` animation
                            // clobbering it on this tick.
                        } else {
                            mid_turn_notice_until = None;
                            status = Status::Thinking { elapsed: turn_started.elapsed() };
                        }
                        // History is never redrawn from here (see the
                        // comment above `agent_future`) — only the status
                        // row's shimmer/elapsed time animates, so this
                        // only needs the footer redrawn, never a new
                        // `append_new_history` call.
                        if draw_chat_frame(
                            &mut terminal.borrow_mut(),
                            &input_buffer,
                            input_cursor,
                            suggestions.len(),
                            |f| {
                                draw_frame(
                                    f,
                                    &conversation_view.history,
                                    &mut conversation_view.history_wrap_cache,
                                    conversation_view.scroll_offset,
                                    &input_buffer,
                                    input_cursor,
                                    &status,
                                    &render::HeaderInfo {
                                        provider_name: &provider_name,
                                        model_name: &model_name,
                                        usage: cumulative_usage,
                                        cwd: &cwd_display,
                                        cwd_short: &cwd_footer_display,
                                        effort_name: &effort_name,
                                    },
                                    &suggestions,
                                    selected_suggestion,
                                    &conversation_view.selection,
                                )
                            },
                        )
                            .is_err()
                        {
                            break TurnOutcome::Fatal;
                        }
                        rendered_suggestion_count = suggestions.len();
                    }
                    maybe_event = event_stream.next() => {
                        let maybe_event = maybe_event.map(|event| event.and_then(crate::terminal::filter_event));
                        use ratatui::crossterm::event::{
                            Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
                            MouseEventKind,
                        };
                        #[cfg(target_os = "macos")]
                        if let Some(Ok(Event::Mouse(mouse))) = &maybe_event {
                            if let Some(notice) = reveal_history_file(
                                &mut conversation_view,
                                *mouse,
                                &args.cwd,
                                ratatui::layout::Rect::new(
                                    0, 0, terminal_width(&terminal),
                                    area_height_for_history(render::footer_height(rendered_suggestion_count), terminal_height(&terminal)),
                                ),
                            ) {
                                status = notice;
                                mid_turn_notice_until = Some(Instant::now() + MID_TURN_NOTICE_DURATION);
                            }
                        } else {
                            conversation_view.file_click.cancel();
                        }
                        match maybe_event {
                            Some(Ok(Event::Paste(mut text))) => {
                                let result = apply_composer_paste(&mut input_buffer, &mut input_cursor, &mut input_history, &text);
                                zeroize::Zeroize::zeroize(&mut text);
                                file_input_changed_at = Instant::now();
                                match result {
                                    Ok(true) => status = Status::Notice("File link inserted".into()),
                                    Err(error) => status = Status::Error(error.into()),
                                    _ => {},
                                }
                                mid_turn_notice_until = Some(Instant::now() + MID_TURN_NOTICE_DURATION);
                            }
                            Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                                file_input_changed_at = Instant::now();
                                if !is_secret_command(&input_buffer)
                                    && matches!(key.code, KeyCode::Backspace | KeyCode::Delete | KeyCode::Left
                                        | KeyCode::Right | KeyCode::Enter | KeyCode::Up | KeyCode::Down)
                                {
                                    file_input::normalize_tail(&mut input_buffer, &mut input_cursor);
                                }
                                match slash_completion_action(&suggestions, &mut selected_suggestion, key) {
                                    SlashCompletionAction::Navigate => continue,
                                    SlashCompletionAction::Submit(command) => {
                                        input_buffer = format!("/{command}");
                                        input_cursor = input_buffer.len();
                                        selected_suggestion = 0;
                                    }
                                    SlashCompletionAction::PassThrough => {}
                                }
                                // Esc interrupts the in-flight turn;
                                // PageUp/PageDown still scroll the
                                // conversation history while a turn is
                                // running, the same saturating-add/sub as
                                // the idle-loop handler above — previously
                                // every key but Esc was silently discarded
                                // here. Ctrl+O copies the last reply, same
                                // as the idle-loop handler.
                                match key.code {
                                    KeyCode::Esc => {
                                        #[cfg(unix)]
                                        {
                                            let context = runtime_commands::RuntimeMutationContext {effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted:true};
                                            match runtime_effects::begin(&context,"D23",durable_session_id.as_bytes()) {
                                                Ok(started)=>interruption_effect=started,
                                                Err(error)=>{*runtime_error.borrow_mut()=Some(error);break TurnOutcome::Fatal;}
                                            }
                                        }
                                        break TurnOutcome::Interrupted;
                                    },
                                    KeyCode::PageUp => {
                                        conversation_view.scroll_offset =
                                            conversation_view.scroll_offset.saturating_add(1);
                                    }
                                    KeyCode::PageDown => {
                                        conversation_view.scroll_offset =
                                            conversation_view.scroll_offset.saturating_sub(1);
                                    }
                                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        #[cfg(unix)]
                                        { status = Status::Notice("After execution finishes, confirm copying with /copy".into()); }
                                        #[cfg(not(unix))]
                                        {
                                            status = match require_action_owner(&slash::Action::Copy) {
                                                Ok(()) => Status::Notice(clipboard::copy_text_with(reply_before_this_turn.as_deref(), clipboard::copy_to_clipboard)),
                                                Err(error) => Status::Error(error.into()),
                                            };
                                        }
                                        // Draw immediately so the
                                        // confirmation appears right away
                                        // rather than up to 100ms late,
                                        // and set the deadline the ticker
                                        // branch checks so it keeps
                                        // showing this instead of
                                        // clobbering it with `Thinking` on
                                        // its very next tick.
                                        mid_turn_notice_until =
                                            Some(Instant::now() + MID_TURN_NOTICE_DURATION);
                                        if draw_chat_frame(
                                            &mut terminal.borrow_mut(),
                                            &input_buffer,
                                            input_cursor,
                                            suggestions.len(),
                                            |f| {
                                                draw_frame(
                                                    f,
                                                    &conversation_view.history,
                                                    &mut conversation_view.history_wrap_cache,
                                                    conversation_view.scroll_offset,
                                                    &input_buffer,
                                                    input_cursor,
                                                    &status,
                                                    &render::HeaderInfo {
                                                        provider_name: &provider_name,
                                                        model_name: &model_name,
                                                        usage: cumulative_usage,
                                                        cwd: &cwd_display,
                                                        cwd_short: &cwd_footer_display,
                                                        effort_name: &effort_name,
                                                    },
                                                    &suggestions,
                                                    selected_suggestion,
                                                    &conversation_view.selection,
                                                )
                                            },
                                        )
                                            .is_err()
                                        {
                                            break TurnOutcome::Fatal;
                                        }
                                        rendered_suggestion_count = suggestions.len();
                                    }
                                    #[cfg(unix)]
                                    KeyCode::Enter if input_buffer.starts_with("/instruction ") => {
                                        let request=std::mem::take(&mut input_buffer);input_cursor=0;
                                        let result=(||->Result<String,String>{
                                            let text=request.strip_prefix("/instruction send ").ok_or("Usage: /instruction send <instruction-for-next-run>")?;
                                            let active=tui_admission.as_mut().ok_or("No saved admission owner")?;
                                            let saved=runtime_persistence.as_ref().ok_or("A saved v2 conversation is required")?;
                                            let handle=instruction_commands::receive(&active.store,&mut active.manager,saved,&management_effects,&_project_activity,polaris_core::management_effect::ManagementOperationIdV1::random().map_err(|e|e.to_string())?,text)?;
                                            let id=handle.admission_id().as_str().to_owned();queued_instructions.push_back(handle);Ok(id)
                                        })();
                                        match result {Ok(id)=>{conversation_view.push_history_line(render::HistoryLine{line:ratatui::text::Line::from(format!("Instruction admission ID: {id}. Applied at this conversation's next run boundary")),shaded:true});status=Status::Notice("Instruction saved for the next run".into());},Err(error)=>status=Status::Error(error)}
                                        mid_turn_notice_until=Some(Instant::now()+MID_TURN_NOTICE_DURATION);
                                    }
                                    #[cfg(unix)]
                                    KeyCode::Enter if input_buffer.starts_with("/input ") => {
                                        let request=std::mem::take(&mut input_buffer);input_cursor=0;
                                        let result=(|| -> Result<(bool,String),String> {
                                            let words=request.split_whitespace().collect::<Vec<_>>();
                                            let ["/input",verb,id]=words.as_slice() else {return Err("Usage: /input status|cancel <admission-id>".into());};
                                            let active=tui_admission.as_ref().ok_or("This run has no saved admission ID")?;
                                            let id=polaris_core::session_manager::AdmissionId::new(*id).map_err(|e|e.to_string())?;
                                            let receipt=active.manager.status(&id).map_err(|e|e.to_string())?;
                                            if *verb=="status" {return Ok((false,render::sanitize(&format!("Input {} / revision {} / state {:?}",receipt.id.as_str(),receipt.revision,receipt.state))));}
                                            if *verb!="cancel" {return Err("Usage: /input status|cancel <admission-id>".into());}
                                            if id!=active.admission_id {
                                                let position=queued_instructions.iter().position(|item|item.admission_id()==&id).ok_or("This is not an active or queued input ID for this conversation")?;
                                                #[cfg(target_os="linux")] let mut reader=LinuxSignalAwareKeyReader;
                                                #[cfg(not(target_os="linux"))] let mut reader=approver::CrosstermKeyReader;
                                                if !account::confirm(&terminal,&mut reader,"Cancel queued input",id.as_str(),"Cancel only the unexecuted instruction with this ID.") {
                                                    let context=runtime_commands::RuntimeMutationContext{effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted:false};
                                                    runtime_effects::begin(&context,"D10",id.as_str().as_bytes())?;
                                                    return Ok((false,"Cancellation request cancelled".into()));
                                                }
                                                let instruction=queued_instructions.remove(position).expect("queued");
                                                let active=tui_admission.as_mut().ok_or("No stored admission owner")?;
                                                instruction_commands::cancel_queue(&active.store,&mut active.manager,runtime_persistence.as_ref().ok_or("A v2 conversation is required")?,&management_effects,&_project_activity,instruction)?;
                                                return Ok((false,format!("Input {} cancelled",id.as_str())));
                                            }
                                            #[cfg(target_os="linux")]
                                            let mut reader=LinuxSignalAwareKeyReader;
                                            #[cfg(not(target_os="linux"))]
                                            let mut reader=approver::CrosstermKeyReader;
                                            let accepted=account::confirm(&terminal,&mut reader,"Request input stop",id.as_str(),"Stop this input's run and verify termination of its owned children and MCP runtime.");
                                            let context=runtime_commands::RuntimeMutationContext{effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted};
                                            interruption_effect=runtime_effects::begin(&context,"D10",id.as_str().as_bytes())?;
                                            Ok((accepted,if accepted {"Input stop requested".into()}else{"Stop request cancelled".into()}))
                                        })();
                                        match result {
                                            Ok((true,_))=>break TurnOutcome::Interrupted,
                                            Ok((false,line))=>{conversation_view.push_history_line(render::HistoryLine{line:ratatui::text::Line::from(line),shaded:false});status=Status::Notice("Input state displayed".into());},
                                            Err(error)=>status=Status::Error(error),
                                        }
                                        mid_turn_notice_until=Some(Instant::now()+MID_TURN_NOTICE_DURATION);
                                    }
                                    #[cfg(unix)]
                                    KeyCode::Enter if input_buffer.starts_with("/subagents") || input_buffer.starts_with("/run details") => {
                                        let request=std::mem::take(&mut input_buffer);
                                        input_cursor=0;
                                        let result = if let Some(extra)=request.strip_prefix("/subagents") {
                                            #[cfg(target_os="linux")]
                                            let mut cancellation_reader=LinuxSignalAwareKeyReader;
                                            #[cfg(not(target_os="linux"))]
                                            let mut cancellation_reader=approver::CrosstermKeyReader;
                                            let accepted = !extra.trim().starts_with("cancel ") || account::confirm(&terminal,&mut cancellation_reader,"Request subagent stop",extra.trim(),"Send a stop request only to the current child/attempt. Verify termination with wait.");
                                            runtime_commands::subagents(Some(&turn_control),extra.trim(),Some(runtime_commands::RuntimeMutationContext{effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted})).await
                                        } else {
                                            runtime_commands::run_details(Some(&turn_control)).await.map(|mut lines| { lines.extend(conversation_view.tool_detail_lines()); lines })
                                        };
                                        match result {
                                            Ok(lines)=>{conversation_view.extend_text_lines(lines);status=Status::Notice("Execution state displayed".into());},
                                            Err(error)=>status=Status::Error(error),
                                        }
                                        mid_turn_notice_until=Some(Instant::now()+MID_TURN_NOTICE_DURATION);
                                    }
                                    _ => match apply_inflight_key(
                                        &mut input_buffer,
                                        &mut input_cursor,
                                        &mut input_history,
                                        &mut queued_inputs,
                                        key,
                                    ) {
                                        InflightInputAction::Continue => { selected_suggestion = 0; }
                                        InflightInputAction::Queued => {
                                            #[cfg(unix)]
                                            if queued_inputs.back().is_some_and(|text|!text.starts_with('/')) && runtime_persistence.is_some() {
                                                let text=queued_inputs.pop_back().expect("queued input");
                                                let result=(||->Result<String,String>{
                                                    let active=tui_admission.as_mut().ok_or("No saved admission owner")?;
                                                    let handle=instruction_commands::receive(&active.store,&mut active.manager,runtime_persistence.as_ref().expect("saved"),&management_effects,&_project_activity,polaris_core::management_effect::ManagementOperationIdV1::random().map_err(|e|e.to_string())?,&text)?;
                                                    let id=handle.admission_id().as_str().to_owned();queued_instructions.push_back(handle);Ok(id)
                                                })();
                                                match result {Ok(id)=>status=Status::Notice(format!("Input {id} saved. Applied in the next run")),Err(error)=>{input_buffer=text;input_cursor=input_buffer.len();status=Status::Error(error);}}
                                            } else {
                                                status=Status::Notice(format!("queued local command ({})",queued_inputs.len()));
                                            }
                                            #[cfg(not(unix))]
                                            {status = Status::Notice(format!("queued input ({})",queued_inputs.len()));}
                                            mid_turn_notice_until = Some(
                                                Instant::now() + MID_TURN_NOTICE_DURATION,
                                            );
                                        }
                                        InflightInputAction::RejectedSecret => {
                                            status = Status::Error(
                                                "This TUI does not yet accept secret configuration".into(),
                                            );
                                            mid_turn_notice_until = Some(
                                                Instant::now() + MID_TURN_NOTICE_DURATION,
                                            );
                                        }
                                        InflightInputAction::RejectedUnavailable => {
                                            status = Status::Error(WINDOWS_OWNER_UNAVAILABLE.into());
                                            mid_turn_notice_until = Some(
                                                Instant::now() + MID_TURN_NOTICE_DURATION,
                                            );
                                        }
                                        InflightInputAction::Interrupt => {
                                            break TurnOutcome::Interrupted;
                                        }
                                    },
                                }
                            }
                            Some(Ok(Event::Mouse(mouse))) if screen_selection::link_modifier(mouse.modifiers)
                                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)) => {
                                    conversation_view.selection = None;
                                }
                            Some(Ok(Event::Mouse(mouse))) => match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    conversation_view.scroll_offset = conversation_view
                                        .scroll_offset
                                        .saturating_add(MOUSE_SCROLL_LINES);
                                }
                                MouseEventKind::ScrollDown => {
                                    conversation_view.scroll_offset = conversation_view
                                        .scroll_offset
                                        .saturating_sub(MOUSE_SCROLL_LINES);
                                }
                                MouseEventKind::Down(MouseButton::Left) => {
                                    // Input may change before the next ticker redraw.
                                    // Hit-test the footer that is actually on screen.
                                    let history_area_height = area_height_for_history(
                                        render::footer_height(rendered_suggestion_count),
                                        terminal_height(&terminal),
                                    );
                                    if mouse.row < history_area_height {
                                        let width = terminal_width(&terminal);
                                        let wrapped = render::wrap_history_lines(
                                            &conversation_view.history,
                                            width,
                                        );
                                        let window = render::visible_history_window(
                                            wrapped.len(),
                                            conversation_view.scroll_offset,
                                            history_area_height as usize,
                                        );
                                        let pos = selection::text_pos_from_screen(
                                            &wrapped,
                                            window,
                                            ratatui::layout::Rect::new(
                                                0,
                                                0,
                                                width,
                                                history_area_height,
                                            ),
                                            mouse.row,
                                            mouse.column,
                                        );
                                        conversation_view.selection = Some(selection::Selection {
                                            anchor: pos,
                                            cursor: pos,
                                            dragging: true,
                                        });
                                    }
                                }
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    if let Some(sel) = conversation_view.selection.as_mut()
                                        && sel.dragging
                                    {
                                        let history_area_height = area_height_for_history(
                                            render::footer_height(rendered_suggestion_count),
                                            terminal_height(&terminal),
                                        );
                                        let width = terminal_width(&terminal);
                                        let wrapped = render::wrap_history_lines(
                                            &conversation_view.history,
                                            width,
                                        );
                                        let window = render::visible_history_window(
                                            wrapped.len(),
                                            conversation_view.scroll_offset,
                                            history_area_height as usize,
                                        );
                                        sel.cursor = selection::text_pos_from_screen(
                                            &wrapped,
                                            window,
                                            ratatui::layout::Rect::new(
                                                0,
                                                0,
                                                width,
                                                history_area_height,
                                            ),
                                            mouse.row,
                                            mouse.column,
                                        );
                                        if mouse.row == 0 {
                                            conversation_view.scroll_offset =
                                                conversation_view.scroll_offset.saturating_add(1);
                                        } else if mouse.row.saturating_add(1) >= history_area_height
                                        {
                                            conversation_view.scroll_offset =
                                                conversation_view.scroll_offset.saturating_sub(1);
                                        }
                                    }
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    if let Some(sel) = conversation_view.selection.as_mut()
                                        && sel.dragging
                                    {
                                        sel.dragging = false;
                                        let width = terminal_width(&terminal);
                                        let wrapped = render::wrap_history_lines(
                                            &conversation_view.history,
                                            width,
                                        );
                                        let text = selection::extract_text(&wrapped, sel);
                                        if !text.is_empty() {
                                            #[cfg(unix)]
                                            { status = Status::Notice("Confirm copying the selected range after execution finishes".into()); }
                                            #[cfg(not(unix))]
                                            { status = Status::Notice(apply_selection_copy(&text, clipboard::copy_to_clipboard)); }
                                            mid_turn_notice_until = Some(
                                                Instant::now() + MID_TURN_NOTICE_DURATION,
                                            );
                                            if draw_chat_frame(
                                                &mut terminal.borrow_mut(),
                                                &input_buffer,
                                                input_cursor,
                                                suggestions.len(),
                                                |f| {
                                                    draw_frame(
                                                        f,
                                                        &conversation_view.history,
                                                        &mut conversation_view.history_wrap_cache,
                                                        conversation_view.scroll_offset,
                                                        &input_buffer,
                                                        input_cursor,
                                                        &status,
                                                        &render::HeaderInfo {
                                                            provider_name: &provider_name,
                                                            model_name: &model_name,
                                                            usage: cumulative_usage,
                                                            cwd: &cwd_display,
                                                            cwd_short: &cwd_footer_display,
                                                            effort_name: &effort_name,
                                                        },
                                                        &suggestions,
                                                        selected_suggestion,
                                                        &conversation_view.selection,
                                                    )
                                                },
                                            )
                                                .is_err()
                                            {
                                                break TurnOutcome::Fatal;
                                            }
                                            rendered_suggestion_count = suggestions.len();
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Some(Ok(Event::Resize(_, _))) => {
                                conversation_view.selection = None;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break TurnOutcome::Fatal,
                        }
                    }
                }
            };

            // `biased` makes `agent_future` win whenever it and
            // `events_rx` are ready in the same poll, so the very last
            // events of a turn can still be sitting in the queue here —
            // print them rather than silently dropping the final tool's
            // result marker.
            while let Ok(event) = events_rx.try_recv() {
                #[cfg(unix)]
                rewind_effects.observe(&event, |path| {
                    rewind_workspace
                        .as_ref()
                        .is_some_and(|owner| owner.is_restorable_path(path).unwrap_or(false))
                });
                update_effort_from_event(&event, &mut effort_name);
                conversation_view.append_live_event(&event);
                if let polaris_core::AgentEvent::HistoryCompacted {
                    messages_before,
                    messages_after,
                    ..
                } = event
                {
                    history_compacted_this_turn = true;
                    checkpoint =
                        checkpoint.saturating_sub(messages_before.saturating_sub(messages_after));
                    conversation_view.observe_compaction(messages_before, messages_after);
                }
            }
            turn_outcome
        };
        #[cfg(target_os = "linux")]
        let shutdown_reason = match &outcome {
            TurnOutcome::Shutdown(reason) => Some(*reason),
            _ => signal::pending_linux_process_shutdown(),
        };
        #[cfg(target_os = "linux")]
        let mcp_cleanup_error = if let Some(runtime) = mcp_runtime.as_mut() {
            // An unconfirmed stop parks the child with the shared cleanup owner.
            runtime
                .shutdown()
                .await
                .err()
                .map(|error| format!("Cannot stop the MCP runtime: {error}"))
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        if let Some(runtime) = mcp_runtime.as_mut() {
            // An unconfirmed stop parks the child with the shared cleanup owner.
            let _ = runtime.shutdown().await;
        }
        #[cfg(target_os = "linux")]
        let shutdown_reason = shutdown_reason.or_else(signal::pending_linux_process_shutdown);
        // Only a normally completed root turn can prove that every child
        // reached a terminal state.  Provider/tool errors can abandon a
        // spawn wave just as interruption can, so fail closed and mark any
        // still-running child outcome unknown before releasing the owner.
        #[cfg(target_os = "linux")]
        let cancel_children = linux_cancel_owned_children(
            shutdown_reason,
            matches!(&outcome, TurnOutcome::Done(Ok(_))),
        );
        #[cfg(not(target_os = "linux"))]
        let cancel_children = !matches!(&outcome, TurnOutcome::Done(Ok(_)));
        let mut control_cleanup_error = session
            .finish_root_delegation_control(cancel_children)
            .await
            .err();
        #[cfg(target_os = "linux")]
        let shutdown_reason = shutdown_reason.or_else(signal::pending_linux_process_shutdown);
        #[cfg(unix)]
        if let Some(error) = runtime_error.borrow_mut().take() {
            control_cleanup_error = Some(format!(
                "Failed to save execution management state: {error}"
            ));
        }
        #[cfg(target_os = "linux")]
        if control_cleanup_error.is_none() {
            control_cleanup_error = mcp_cleanup_error.clone();
        }
        if let Some(admission) = tui_admission.take() {
            let terminal = match &outcome {
                TurnOutcome::Done(Ok(_)) => {
                    polaris_core::session_manager::TerminalStatus::Completed
                }
                TurnOutcome::Done(Err(_)) => polaris_core::session_manager::TerminalStatus::Failed,
                TurnOutcome::Interrupted => {
                    polaris_core::session_manager::TerminalStatus::Cancelled
                }
                TurnOutcome::Fatal => polaris_core::session_manager::TerminalStatus::OutcomeUnknown,
                #[cfg(target_os = "linux")]
                TurnOutcome::Shutdown(_) => {
                    polaris_core::session_manager::TerminalStatus::OutcomeUnknown
                }
            };
            #[cfg(target_os = "linux")]
            let terminal = linux_admission_status_after_cleanup(
                terminal,
                shutdown_reason.or_else(signal::pending_linux_process_shutdown),
            );
            let terminal = if control_cleanup_error.is_some() {
                polaris_core::session_manager::TerminalStatus::OutcomeUnknown
            } else {
                terminal
            };
            if let Err(error) = admission.finish(terminal) {
                control_cleanup_error = Some(format!("Cannot stop the sidechat run: {error}"));
            }
        }
        #[cfg(target_os = "linux")]
        if control_cleanup_error.is_none() {
            control_cleanup_error = mcp_cleanup_error;
        }
        #[cfg(unix)]
        {
            if let Some(error) = runtime_error.into_inner() {
                control_cleanup_error = Some(format!(
                    "Failed to save execution management state: {error}"
                ));
            }
            if let Some(started) = interruption_effect {
                let context = runtime_commands::RuntimeMutationContext {
                    effects: &management_effects,
                    lease: &_project_activity,
                    session_id: &durable_session_id,
                    generation: runtime_generation,
                    accepted: true,
                };
                if let Err(error) = runtime_effects::finish(
                    &context,
                    &started,
                    control_cleanup_error.is_none(),
                    b"root-interrupted-after-owned-cleanup",
                ) {
                    control_cleanup_error = Some(format!("Cannot save the stop outcome: {error}"));
                }
            }
        }
        #[cfg(target_os = "linux")]
        let mut cleanup_failure = control_cleanup_error.clone();
        let turn_completed = matches!(&outcome, TurnOutcome::Done(Ok(_)));
        #[cfg(unix)]
        let turn_interrupted = matches!(&outcome, TurnOutcome::Interrupted);
        #[cfg(target_os = "linux")]
        let turn_completed = linux_turn_completed_without_shutdown(
            turn_completed,
            shutdown_reason.or_else(signal::pending_linux_process_shutdown),
        );
        if turn_completed && let Some(finalizer) = args.answer_finalizer.as_ref() {
            let scheduled = schedule_tui_shadow(finalizer, &session, &text, &turn_control);
            if let Err(error) = scheduled {
                local_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                    format!("Cannot record answer completion assessment: {error}"),
                    ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
                )));
            }
        }

        match outcome {
            TurnOutcome::Done(Ok(result)) => {
                let _ = result;
                status = Status::Idle;
                if session.persistence.is_none() && history_compacted_this_turn {
                    if let Err(e) = persist::rewrite(&session_path, &session.messages) {
                        fatal_message = Some(format!("Can't persist the compacted session: {e}"));
                        break 'outer ExitCode::FAILURE;
                    }
                } else if session.persistence.is_none()
                    && let Some(reply) = session.messages.last()
                    && let Err(e) = persist::append_message(&session_path, reply)
                {
                    fatal_message = Some(format!("Can't persist the reply: {e}"));
                    break 'outer ExitCode::FAILURE;
                }
                history_compacted_this_turn = false;
            }
            TurnOutcome::Done(Err(e)) => {
                // The agent loop can return after recording an assistant
                // message with tool_calls but before every matching
                // tool-result message is pushed (see AgentError::Stopped /
                // AgentError::Io in polaris_core::agent::run). Re-sending
                // that unbalanced tail to the provider on the next turn
                // would be rejected every time, so roll `session.messages`
                // back to right after the user's message — the only state
                // that was ever actually persisted for this turn.
                status = Status::Error(e.to_string());
                if let Err(error) = keep_or_roll_back(&mut session, checkpoint) {
                    status = Status::Error(error.to_string());
                }
            }
            TurnOutcome::Interrupted => {
                // Same rollback as the error case above — an interrupted
                // turn can equally have left an unbalanced tool-call tail.
                status = Status::Notice("interrupted".to_string());
                if let Err(error) = keep_or_roll_back(&mut session, checkpoint) {
                    status = Status::Error(error.to_string());
                }
            }
            #[cfg(target_os = "linux")]
            TurnOutcome::Shutdown(_) => {
                if let Err(error) = keep_or_roll_back(&mut session, checkpoint) {
                    cleanup_failure =
                        Some(format!("Cannot recover the stopping conversation: {error}"));
                }
            }
            TurnOutcome::Fatal => {
                if let (Some(boundary), Some(persistence)) =
                    (rewind_boundary.take(), session.persistence.as_ref())
                {
                    let _ = persistence.abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "TUI execution exited before completion".into(),
                    );
                }
                break ExitCode::FAILURE;
            }
        }
        #[cfg(unix)]
        let rewind_writers_stopped = control_cleanup_error.is_none();
        if let Some(error) = control_cleanup_error {
            status = Status::Error(format!("Cannot stop subagent control: {error}"));
        }
        #[cfg(target_os = "linux")]
        let rewind_turn_completed = linux_turn_completed_without_shutdown(
            turn_completed,
            shutdown_reason.or_else(signal::pending_linux_process_shutdown),
        );
        #[cfg(all(unix, not(target_os = "linux")))]
        let rewind_turn_completed = turn_completed;
        #[cfg(unix)]
        drop(_project_activity);
        #[cfg(unix)]
        if let Some(boundary) = rewind_boundary {
            let Some(persistence) = session.persistence.as_ref() else {
                fatal_message = Some("Rewind boundary storage source was lost".into());
                break 'outer ExitCode::FAILURE;
            };
            let completion = if rewind_turn_completed && rewind_writers_stopped {
                match rewind_workspace.as_ref() {
                    Some(owner) => finish_rewind_for_session(
                        owner,
                        persistence,
                        &management_effects,
                        &args.sandbox,
                        &boundary,
                        rewind_effects.unknown_reason(),
                        || {
                            #[cfg(target_os = "linux")]
                            {
                                linux_turn_completed_without_shutdown(
                                    true,
                                    signal::pending_linux_process_shutdown(),
                                )
                            }
                            #[cfg(not(target_os = "linux"))]
                            {
                                true
                            }
                        },
                    ),
                    None => persistence.abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "File storage is unavailable".into(),
                    ),
                }
            } else {
                let reason = if turn_interrupted && rewind_writers_stopped {
                    "Interrupted turns cannot be rewound (save boundary incomplete)"
                } else {
                    "The turn or a subagent did not finish successfully"
                };
                persistence.abandon_rewind_boundary(&boundary.boundary_id, reason.into())
            };
            match completion {
                Ok(saved) => {
                    rewind_unavailable_reason = match saved.completion {
                        Some(completion) if matches!(
                            completion.eligibility,
                            polaris_core::conversation_state::RewindEligibilityV1::Eligible
                        ) => "No saved rewind boundaries yet".into(),
                        Some(completion) => match completion.effect {
                            polaris_core::conversation_state::RewindEffectStatusV1::Unknown {
                                description,
                            }
                            | polaris_core::conversation_state::RewindEffectStatusV1::ExternalMutation {
                                description,
                            } => description,
                            _ => "This turn cannot be rewound".into(),
                        },
                        None => saved
                            .abandoned_reason
                            .unwrap_or_else(|| "This turn's save boundary is incomplete".into()),
                    };
                }
                Err(error) => {
                    fatal_message = Some(format!("Cannot finalize the rewind boundary: {error}"));
                    break 'outer ExitCode::FAILURE;
                }
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(reason) = shutdown_reason.or_else(signal::pending_linux_process_shutdown) {
            if let Some(error) = cleanup_failure {
                fatal_message = Some(error);
                break 'outer ExitCode::FAILURE;
            }
            break 'outer linux_signal_exit_code(reason);
        }
    };

    #[cfg(unix)]
    let exit_code = if session.persistence.is_some() && !last_submission_was_local {
        match draft::save(
            &session,
            &initial_saved_settings,
            rewind_workspace.as_ref().expect("startup owner"),
            &management_effects,
            input_history.pending_draft(&input_buffer),
        ) {
            Ok(()) => exit_code,
            Err(error) => {
                fatal_message = Some(match fatal_message.take() {
                    Some(previous) => format!("{previous}; draft storage: {error}"),
                    None => format!("Cannot save the draft: {error}"),
                });
                ExitCode::FAILURE
            }
        }
    } else {
        exit_code
    };
    #[cfg(target_os = "linux")]
    let exit_code = match source_frontend {
        Ok(owner) => match owner.shutdown().await {
            Ok(()) => exit_code,
            Err(error) => {
                fatal_message = Some(error);
                ExitCode::FAILURE
            }
        },
        Err(_) => exit_code,
    };
    #[cfg(target_os = "linux")]
    let (exit_code, restoration_error) = linux_exit_after_terminal_restoration(
        exit_code,
        terminal_guard.restore(),
        signal::pending_linux_process_shutdown(),
    );
    #[cfg(target_os = "linux")]
    if let Some(error) = restoration_error {
        fatal_message = Some(match fatal_message.take() {
            Some(previous) => format!("{previous}; {error}"),
            None => error,
        });
    }
    // ratatui restores the alternate screen on the other platforms.
    #[cfg(not(target_os = "linux"))]
    crate::terminal::restore();
    // Restore the terminal's own default cursor shape, undoing the
    // BlinkingBar set at startup — best-effort, same as the set itself.
    #[cfg(not(target_os = "linux"))]
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    // Undoes the EnableMouseCapture set at startup — best-effort, same as
    // the enable itself. Otherwise mouse reporting mode would leak into
    // whatever the user's shell does next (e.g. text selection with the
    // mouse would stop working until they open and close another
    // mouse-reporting program).
    #[cfg(not(target_os = "linux"))]
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste,
        ratatui::crossterm::event::DisableMouseCapture
    );
    if let Some(msg) = fatal_message {
        eprintln!("{msg}");
    }
    if let Some(session_id) = current_resume_id(&session, &session_path) {
        print_resume_hint(&session_id);
    }
    exit_code
}

/// Runs the `/resume` picker as its own small blocking loop on the same
/// terminal — the same approach the onboarding screen uses for a
/// full-screen selection UI. Its own reads are still simple blocking
/// ones — unlike the agent-turn loop, nothing here needs to animate
/// concurrently — but `terminal` is still a shared `&RefCell` rather
/// than `&mut` for consistency with the rest of `run()`, which never
/// gives up its own borrow of it. Generic over `Backend`/`KeyReader` for
/// the same reason `TuiApprover` is: so it's testable without a real
/// terminal. Returns the chosen conversation's id, or `None` if the user
/// backed out with Esc (or there was nothing to resume and any key was
/// pressed to dismiss the empty state).
fn run_resume_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    groups: &[sessions::DirGroup],
) -> Option<sessions::SessionSummary> {
    let flat: Vec<&sessions::SessionSummary> =
        groups.iter().flat_map(|g| g.sessions.iter()).collect();
    let now = now_millis();

    if flat.is_empty() {
        let _ = terminal
            .borrow_mut()
            .draw(|f| render::render_resume_picker(f, groups, 0, now));
        let _ = reader.read_key();
        return None;
    }

    let mut selected = 0usize;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_resume_picker(f, groups, selected, now))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % flat.len(),
            Ok(KeyCode::Up) => selected = (selected + flat.len() - 1) % flat.len(),
            Ok(KeyCode::Enter) => return Some(flat[selected].clone()),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn show_read_only_transcript<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    messages: &[polaris_provider::Message],
    source: &str,
) {
    let unwrapped = render::transcript_lines_for(messages);
    let mut offset = 0usize;
    loop {
        let Ok(size) = terminal.borrow().size() else {
            return;
        };
        let wrapped = render::wrap_history_lines(&unwrapped, size.width.saturating_sub(2));
        let page = size.height.saturating_sub(4).max(1) as usize;
        let maximum = wrapped.len().saturating_sub(page);
        offset = offset.min(maximum);
        if terminal
            .borrow_mut()
            .draw(|frame| render::render_transcript_viewer(frame, &wrapped, offset, source))
            .is_err()
        {
            return;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Up) => offset = offset.saturating_sub(1),
            Ok(KeyCode::Down) => offset = (offset + 1).min(maximum),
            Ok(KeyCode::PageUp) => offset = offset.saturating_sub(page),
            Ok(KeyCode::PageDown) => offset = offset.saturating_add(page).min(maximum),
            Ok(KeyCode::Home) => offset = 0,
            Ok(KeyCode::End) => offset = maximum,
            Ok(KeyCode::Esc | KeyCode::Enter | KeyCode::Null) => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn run_rewind_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    rows: &[render::RewindPickerRow],
    unavailable_reason: &str,
) -> Option<String> {
    let now = now_millis();
    if rows.is_empty() {
        let _ = terminal.borrow_mut().draw(|f| {
            render::render_rewind_picker_with_reason(f, rows, 0, now, unavailable_reason)
        });
        let _ = reader.read_key();
        return None;
    }
    let mut selected = 0usize;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| {
                render::render_rewind_picker_with_reason(f, rows, selected, now, unavailable_reason)
            })
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % rows.len(),
            Ok(KeyCode::Up) => selected = (selected + rows.len() - 1) % rows.len(),
            Ok(KeyCode::Enter) => return Some(rows[selected].boundary_id.clone()),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn confirm_rewind_preview<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    preview: &render::RewindPreviewView,
) -> bool {
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_rewind_preview(f, preview))
            .is_err()
        {
            return false;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Char('r')) => return true,
            Ok(KeyCode::Esc) | Err(_) => return false,
            Ok(_) => continue,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone, Copy)]
enum C10TuiInvocation {
    Push,
    Recover(polaris_core::management_effect::ManagementOperationIdV1),
    Abandon(polaris_core::management_effect::ManagementOperationIdV1),
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn c10_tui_invocation(requested: &str) -> Result<C10TuiInvocation, String> {
    use polaris_core::management_effect::ManagementOperationIdV1;
    use std::str::FromStr;
    let words: Vec<_> = requested.split_whitespace().collect();
    match words.as_slice() {
        ["push"] => Ok(C10TuiInvocation::Push),
        [action @ ("recover" | "abandon"), operation] => {
            let operation = ManagementOperationIdV1::from_str(operation)
                .map_err(|_| "Invalid C10 operation ID".to_string())?;
            if *action == "recover" {
                Ok(C10TuiInvocation::Recover(operation))
            } else {
                Ok(C10TuiInvocation::Abandon(operation))
            }
        }
        _ => Err("Usage: /sync push · /sync recover <C10-operation-id> · /sync abandon <C10-operation-id>".into()),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn is_c10_sync_request(requested: &str) -> bool {
    let words: Vec<_> = requested.split_whitespace().collect();
    match words.first() {
        Some(&"push" | &"abandon") => true,
        // C12 retains its 64-hex operation key; C10 uses the distinct typed
        // 32-hex management operation ID. No ledger is inferred here.
        Some(&"recover") => !matches!(words.as_slice(), ["recover", key]
            if key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn c10_unavailable_action(requested: &str, has_queued_input: bool) -> Result<String, String> {
    if has_queued_input {
        return Err("Clear queued input before changing shared content".into());
    }
    match c10_tui_invocation(requested)? {
        C10TuiInvocation::Push => {}
        C10TuiInvocation::Recover(operation) | C10TuiInvocation::Abandon(operation) => {
            let _ = operation;
        }
    }
    Err("c10_unavailable_platform: shared sends require the macOS Data Protection Keychain.".into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn record_c10_result(
    result: Result<String, String>,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    match result {
        Ok(text) => {
            local_lines.extend(
                text.lines()
                    .map(|line| ratatui::text::Line::from(render::sanitize(line))),
            );
            *status = Status::Notice("C10 operation result displayed".into());
        }
        Err(error) => *status = Status::Error(render::sanitize(&error)),
    }
}

#[cfg(any(target_os = "macos", test))]
enum C10TuiEvent {
    Preview(render::C10ConfirmView, std::sync::mpsc::SyncSender<bool>),
    Confirm(render::C10ConfirmView, std::sync::mpsc::SyncSender<bool>),
    Finished(Result<String, String>),
}

#[cfg(any(target_os = "macos", test))]
struct C10TuiHost {
    invocation: C10TuiInvocation,
    events: std::sync::mpsc::Sender<C10TuiEvent>,
    ui_open: std::sync::Arc<std::sync::atomic::AtomicBool>,
    abandon_consent: std::sync::Mutex<
        Option<(
            polaris_core::management_effect::ManagementOperationIdV1,
            polaris_core::sync::remote_intent::SyncDigestV1,
        )>,
    >,
}

#[cfg(any(target_os = "macos", test))]
impl C10TuiHost {
    fn live(&self) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        if self.ui_open.load(std::sync::atomic::Ordering::Acquire) {
            Ok(())
        } else {
            Err(polaris_core::sync::push::SyncPushErrorV1::Permission)
        }
    }
    fn ask(
        &self,
        view: render::C10ConfirmView,
    ) -> Result<bool, polaris_core::sync::push::SyncPushErrorV1> {
        use polaris_core::sync::push::SyncPushErrorV1;
        self.live()?;
        let (reply, response) = std::sync::mpsc::sync_channel(0);
        self.events
            .send(C10TuiEvent::Confirm(view, reply))
            .map_err(|_| SyncPushErrorV1::Permission)?;
        let accepted = response.recv().map_err(|_| SyncPushErrorV1::Permission)?;
        self.live()?;
        Ok(accepted)
    }

    fn show_preview(
        &self,
        mut view: render::C10ConfirmView,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        use polaris_core::sync::push::SyncPushErrorV1;
        self.live()?;
        view.title = "C10 preview (confirmation follows preparation)".into();
        let (reply, response) = std::sync::mpsc::sync_channel(0);
        self.events
            .send(C10TuiEvent::Preview(view, reply))
            .map_err(|_| SyncPushErrorV1::Permission)?;
        if !response.recv().map_err(|_| SyncPushErrorV1::Permission)? {
            return Err(SyncPushErrorV1::Permission);
        }
        self.live()
    }
}

#[cfg(any(target_os = "macos", test))]
impl polaris_core::sync::push::SyncPushHostV1 for C10TuiHost {
    fn require_confirmation_path(
        &self,
        _: &polaris_core::sync::push::SyncPushConfirmationSummaryV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        self.live()
    }
    fn require_recovery_invocation(
        &self,
        operation: polaris_core::management_effect::ManagementOperationIdV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        self.live()?;
        if matches!(self.invocation, C10TuiInvocation::Recover(value) if value == operation) {
            Ok(())
        } else {
            Err(polaris_core::sync::push::SyncPushErrorV1::Permission)
        }
    }
    fn confirm_abandon(
        &self,
        operation: polaris_core::management_effect::ManagementOperationIdV1,
        request: polaris_core::sync::remote_intent::SyncDigestV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        use polaris_core::sync::push::SyncPushErrorV1;
        if !matches!(self.invocation, C10TuiInvocation::Abandon(value) if value == operation) {
            return Err(SyncPushErrorV1::Permission);
        }
        if !self.ask(render::C10ConfirmView {
            title: "Reconcile and abandon C10 operation".into(),
            details: vec![
                format!("operation={operation}"),
                format!("request={request}"),
                "This does not prove that nothing was sent.".into(),
            ],
        })? {
            return Err(SyncPushErrorV1::Permission);
        }
        *self
            .abandon_consent
            .lock()
            .map_err(|_| SyncPushErrorV1::Permission)? = Some((operation, request));
        Ok(())
    }
    fn require_abandon_invocation(
        &self,
        operation: polaris_core::management_effect::ManagementOperationIdV1,
        request: polaris_core::sync::remote_intent::SyncDigestV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        use polaris_core::sync::push::SyncPushErrorV1;
        self.live()?;
        if matches!(self.invocation, C10TuiInvocation::Abandon(value) if value == operation)
            && *self
                .abandon_consent
                .lock()
                .map_err(|_| SyncPushErrorV1::Permission)?
                == Some((operation, request))
        {
            Ok(())
        } else {
            Err(SyncPushErrorV1::Permission)
        }
    }
}

#[cfg(any(target_os = "macos", test))]
struct C10TuiJob {
    inbox: Option<std::sync::mpsc::Receiver<C10TuiEvent>>,
    worker: Option<std::thread::JoinHandle<()>>,
    ui_open: std::sync::Arc<std::sync::atomic::AtomicBool>,
    result: Option<Result<String, String>>,
    confirmed: bool,
}

#[cfg(any(target_os = "macos", test))]
impl C10TuiJob {
    fn start(
        invocation: C10TuiInvocation,
        work: impl FnOnce(C10TuiHost) -> Result<String, String> + Send + 'static,
    ) -> Self {
        let (events, inbox) = std::sync::mpsc::channel();
        let ui_open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let host = C10TuiHost {
            invocation,
            events: events.clone(),
            ui_open: ui_open.clone(),
            abandon_consent: std::sync::Mutex::new(None),
        };
        let worker = std::thread::spawn(move || {
            let result = work(host);
            let _ = events.send(C10TuiEvent::Finished(result));
        });
        Self {
            inbox: Some(inbox),
            worker: Some(worker),
            ui_open,
            result: None,
            confirmed: false,
        }
    }

    fn poll(
        &mut self,
        mut preview: impl FnMut(&render::C10ConfirmView) -> bool,
        mut confirm: impl FnMut(&render::C10ConfirmView) -> bool,
    ) -> Option<Result<String, String>> {
        use std::sync::mpsc::TryRecvError;
        if self.result.is_none() {
            match self.inbox.as_ref()?.try_recv() {
                Ok(C10TuiEvent::Preview(view, reply)) => {
                    let _ = reply.send(preview(&view));
                }
                Ok(C10TuiEvent::Confirm(view, reply)) => {
                    let accepted = !self.confirmed && confirm(&view);
                    self.confirmed = true;
                    let _ = reply.send(accepted);
                }
                Ok(C10TuiEvent::Finished(result)) => self.result = Some(result),
                Err(TryRecvError::Disconnected) => {
                    self.result = Some(Err(
                        "The C10 worker ended. Do not resend; inspect state".into()
                    ))
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        // A network child never makes this poll join a running worker.
        if self.result.is_none() || !self.worker.as_ref()?.is_finished() {
            return None;
        }
        self.ui_open
            .store(false, std::sync::atomic::Ordering::Release);
        self.inbox.take();
        if self.worker.take().unwrap().join().is_err() {
            return Some(Err(
                "The C10 worker failed. Do not resend; inspect state".into()
            ));
        }
        Some(
            self.result
                .take()
                .unwrap_or_else(|| Err("Cannot verify the C10 worker result".into())),
        )
    }
}

#[cfg(any(target_os = "macos", test))]
impl Drop for C10TuiJob {
    fn drop(&mut self) {
        self.ui_open
            .store(false, std::sync::atomic::Ordering::Release);
        // Close reply receivers before joining, including a pending prompt.
        self.inbox.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
fn c10_tui_worker(
    invocation: C10TuiInvocation,
    confirm: impl FnOnce(&render::C10ConfirmView) -> bool,
    work: impl FnOnce(C10TuiHost) -> Result<String, String> + Send + 'static,
) -> Result<String, String> {
    let mut job = C10TuiJob::start(invocation, work);
    let mut confirm = Some(confirm);
    loop {
        if let Some(result) = job.poll(
            |_| true,
            |view| confirm.take().is_some_and(|confirm| confirm(view)),
        ) {
            return result;
        }
        std::thread::yield_now();
    }
}

#[cfg(target_os = "macos")]
fn c10_tui_view(
    summary: &polaris_core::sync::push::SyncPushConfirmationSummaryV1,
) -> render::C10ConfirmView {
    render::C10ConfirmView {
        title: "Confirm C10 shared send".into(),
        details: vec![
            format!("operation={}", summary.operation_id),
            format!("request={}", summary.request_hash),
            format!("project={}", summary.project),
            format!("share={}", summary.share_id),
            format!("remote={}", summary.remote_label),
            format!("ref={}", summary.remote_ref),
            format!("manifest={}", summary.manifest_digest),
            format!("payload={}", summary.payload_digest),
            format!("expected remote={:?}", summary.expected_remote),
            format!(
                "included={} excluded={}",
                summary.file_count, summary.excluded_count
            ),
            format!("account registration={}", summary.account_registration_id),
            format!("operation={:?}", summary.action),
            "Conversation rewind cannot undo shared sends.".into(),
        ],
    }
}

#[cfg(any(target_os = "macos", test))]
fn c10_tui_outcome(outcome: polaris_core::sync::push::SyncPushOutcomeV1) -> Result<String, String> {
    use polaris_core::sync::push::SyncPushOutcomeV1;
    match outcome {
        SyncPushOutcomeV1::NoChanges => Ok("C10: shared content matches".into()),
        SyncPushOutcomeV1::Completed {
            operation_id,
            commit,
        } => Ok(format!(
            "C10 send complete: operation={operation_id} commit={commit}"
        )),
        SyncPushOutcomeV1::Denied { operation_id } => {
            Ok(format!("C10 operation cancelled: operation={operation_id}"))
        }
        SyncPushOutcomeV1::ReconciledAbandon { operation_id } => Ok(format!(
            "C10 operation reconciled and abandoned: operation={operation_id} (does not prove that nothing was sent)"
        )),
        SyncPushOutcomeV1::OutcomeUnknown { operation_id } => Err(format!(
            "C10 outcome unknown: operation={operation_id}. Do not resend; reconcile with /sync recover {operation_id}"
        )),
        SyncPushOutcomeV1::PartialComplete { operation_id } => Err(format!(
            "C10 persistence incomplete: operation={operation_id}. Do not resend; reconcile with /sync recover {operation_id}"
        )),
    }
}

#[cfg(target_os = "macos")]
fn start_c10_sync_action(
    project: &Path,
    state: &Path,
    requested: &str,
    has_queued_input: bool,
) -> Result<C10TuiJob, String> {
    if has_queued_input {
        return Err("Clear queued input before changing shared content".into());
    }
    let invocation = c10_tui_invocation(requested)?;
    #[cfg(target_os = "macos")]
    {
        let project = std::fs::canonicalize(polaris_core::project::resolve_root(project))
            .map_err(|_| "Cannot verify the C10 project")?;
        let expected =
            polaris_core::project::state_dir(&project).map_err(|_| "Cannot verify C10 state")?;
        if std::fs::canonicalize(state).map_err(|_| "Cannot verify C10 state")?
            != std::fs::canonicalize(expected).map_err(|_| "Cannot verify C10 state")?
        {
            return Err("C10 project and storage location mismatch".into());
        }
        Ok(C10TuiJob::start(invocation, move |host| {
            use polaris_core::sync::push::SyncPushOwnerV1;
            match invocation {
                C10TuiInvocation::Push => {
                    let ui = C10TuiHost {
                        invocation,
                        events: host.events.clone(),
                        ui_open: host.ui_open.clone(),
                        abandon_consent: std::sync::Mutex::new(None),
                    };
                    let owner = SyncPushOwnerV1::for_project(&project, Box::new(host))
                        .map_err(|e| e.to_string())?;
                    let preview = owner.preview().map_err(|e| e.to_string())?;
                    ui.show_preview(c10_tui_view(preview.summary()))
                        .map_err(|e| e.to_string())?;
                    let token = owner.prepare(preview).map_err(|e| e.to_string())?;
                    // The token stays exclusively on this worker until consumed.
                    let accepted = ui
                        .ask(c10_tui_view(token.summary()))
                        .map_err(|e| e.to_string())?;
                    c10_tui_outcome(
                        if accepted {
                            token.confirm()
                        } else {
                            token.deny()
                        }
                        .map_err(|e| e.to_string())?,
                    )
                }
                C10TuiInvocation::Recover(operation) => {
                    let owner = SyncPushOwnerV1::for_project(&project, Box::new(host))
                        .map_err(|e| e.to_string())?;
                    c10_tui_outcome(owner.recover(operation).map_err(|e| e.to_string())?)
                }
                C10TuiInvocation::Abandon(operation) => {
                    let owner = SyncPushOwnerV1::for_project(&project, Box::new(host))
                        .map_err(|e| e.to_string())?;
                    c10_tui_outcome(owner.abandon(operation).map_err(|e| e.to_string())?)
                }
            }
        }))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn confirm_c10_sync<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    view: &render::C10ConfirmView,
) -> bool {
    let mut offset = 0usize;
    loop {
        let mut ready = false;
        if terminal
            .borrow_mut()
            .draw(|frame| ready = render::render_c10_confirmation(frame, view, offset))
            .is_err()
        {
            return false;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Char('y')) if ready => return true,
            Ok(KeyCode::Down) => offset = offset.saturating_add(1),
            Ok(KeyCode::Up) => offset = offset.saturating_sub(1),
            Ok(KeyCode::Esc) | Err(_) => return false,
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "tests/c10_tui_adapter.rs"]
mod c10_tui_adapter_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_action(
    project: &Path,
    state: &Path,
    requested: &str,
    has_queued_input: bool,
    confirm: impl FnOnce(&render::SyncConfirmView) -> bool,
) -> Result<String, String> {
    use polaris_core::apply_intent::{ApplyInventoryV1, RevisionKindV1, RevisionV1};
    use polaris_core::apply_owner::{ApplyOwnerRecoveryV1, ApplyOwnerV1, C12ApplyAdmissionV1};
    use polaris_core::isolated_workspace::{Limits, RegisteredSecrets};
    use polaris_core::sync::{apply, participation, repository::SyncRepository, share::SyncPaths};
    use std::os::unix::fs::MetadataExt;

    let words: Vec<_> = requested.split_whitespace().collect();
    let inspect = words.is_empty() || words == ["status"];
    let recovering = words.len() == 2 && words[0] == "recover";
    if !inspect
        && words != ["apply"]
        && !recovering
        && !matches!(words.first(), Some(&"join" | &"pull" | &"leave" | &"push"))
    {
        return Err("Usage: /sync [status|join <remote> <share-id> <invite-id> <expires> [excluded-name... ]|pull|leave|apply|recover <displayed-operation-id>]".into());
    }
    if !inspect && has_queued_input {
        return Err("Clear queued input before changing shared content".into());
    }
    let project = std::fs::canonicalize(polaris_core::project::resolve_root(project))
        .map_err(|e| e.to_string())?;
    let expected_state = polaris_core::project::state_dir(&project).map_err(|e| e.to_string())?;
    let state = std::fs::canonicalize(state).map_err(|e| e.to_string())?;
    if state != std::fs::canonicalize(expected_state).map_err(|e| e.to_string())? {
        return Err("Project and storage location mismatch".into());
    }
    // Retain the same owner from inspection through confirmation and commit.
    let owner = ApplyOwnerV1::acquire(&project).map_err(|e| e.to_string())?;
    if matches!(words.first(), Some(&"join" | &"pull" | &"leave" | &"push")) {
        if words[0] == "push" {
            return Err(
                "The shared send owner is not implemented. Sending requires credential verification on a real device.".into(),
            );
        }
        owner
            .ensure_sync_idle()
            .map_err(|_| "Inspect the incomplete sharing operation")?;
        let classification = participation::read(&state).map_err(|e| e.to_string())?;
        let held = classification.participation();
        let mut join = None;
        let (id, detail) = match words[0] {
            "join" if words.len() >= 5 => {
                let share = polaris_core::sync::share::ShareLink {
                    remote: words[1].into(),
                    share_id: words[2].into(),
                    invitation_id: words[3].into(),
                    expires_unix_seconds: words[4]
                        .parse()
                        .map_err(|_| "Specify the invitation expiry in UNIX seconds")?,
                };
                let now =
                    u64::try_from(now_millis() / 1000).map_err(|_| "Cannot verify the invitation expiry")?;
                share
                    .accept(now)
                    .map_err(|_| "Check the invitation URL, ID, and expiry. No changes were made.")?;
                if held.is_some() {
                    return Err("Already participating. The safe credential rebind owner is not implemented.".into());
                }
                let manifest = polaris_core::sync::share::ShareManifest {
                    version: 1,
                    excluded: words[5..].iter().map(|w| (*w).to_owned()).collect(),
                };
                let detail = format!(
                    "Share: {}\nInvitation: {}\nremote: {}\nExpiry: {}\nExcluded: {:?}\nJoining changes only this device's storage. Peers and source files are unchanged.",
                    share.share_id,
                    share.invitation_id,
                    share.remote_display(),
                    share.expires_unix_seconds,
                    manifest.excluded
                );
                join = Some((share, manifest, now));
                ("C08 join", detail)
            }
            "pull" if words.len() == 1 => {
                let held = held.as_ref().ok_or("This project has not joined")?;
                (
                    "C11 pull",
                    format!(
                        "Share: {}\nremote: {}\nAgreed revision: {}\nUpdates only received data and local staging. Source apply requires separate confirmation.",
                        held.share.share_id,
                        held.share.remote_display(),
                        held.agreed_commit.as_deref().unwrap_or("none")
                    ),
                )
            }
            "leave" if words.len() == 1 => {
                let held = held.as_ref().ok_or("This project has not joined")?;
                (
                    "C13 leave",
                    format!(
                        "Share: {}\nRemoves this device's participation state. Shared history, peers, and source files are retained.",
                        held.share.share_id
                    ),
                )
            }
            _ => return Err(
                "/sync join <remote> <share-id> <invite-id> <expires> [excluded-name...] | pull | leave"
                    .into(),
            ),
        };
        let view = render::SyncConfirmView {
            recovering: false,
            project: project.display().to_string(),
            operation: id.into(),
            revision: detail,
            previous_revision: "Bind the prior participation state to the management record".into(),
            recovery: "Source files and peers are unchanged".into(),
            changes: Vec::new(),
        };
        let accepted = confirm(&view);
        return match words[0] {
            "join" => {
                let (share, manifest, now) = join.expect("join validated");
                owner
                    .join_confirmed(share, manifest, now, accepted)
                    .map(|p| {
                        if p.is_some() {
                            "Joined"
                        } else {
                            "Join cancelled"
                        }
                        .into()
                    })
            }
            "pull" => owner.pull_confirmed(accepted).map(|p| match p {
                Some(exchange) => format!(
                    "Received: {} items, {} unresolved. Source files are unchanged.",
                    exchange.changes.len(),
                    exchange.needs_attention().len()
                ),
                None => "Receive cancelled".into(),
            }),
            "leave" => owner.leave_confirmed(accepted).map(|p| {
                if p.is_some() {
                    "This device left the share"
                } else {
                    "Leave cancelled"
                }
                .into()
            }),
            _ => unreachable!(),
        }
        .map_err(|_| {
            "Cannot finalize the sharing operation result. Inspect state with /sync.".into()
        });
    }
    let active = owner.active().map_err(|e| e.to_string())?;
    let recovery = state.join("sync/recovery").display().to_string();
    if inspect {
        let pending = active.as_ref().map_or_else(
            || "none".into(),
            |intent| {
                format!(
                    "{:?} {:?} / operation ID: {}",
                    intent.descriptor().inventory_id,
                    intent.latest().phase,
                    intent.descriptor().operation_key.as_hex()
                )
            },
        );
        let management_pending = match owner
            .inspect_management()
            .map_err(|_| "Cannot verify sharing management records")?
        {
            polaris_core::management_effect::ManagementEffectInspectOpenV1::Absent => {
                "none".to_owned()
            }
            polaris_core::management_effect::ManagementEffectInspectOpenV1::Present(inspected) => {
                let records: Vec<_> = inspected
                    .effects()
                    .filter(|e| e.state.blocks_new_work())
                    .map(|e| {
                        format!(
                            "{} {} {:?}",
                            e.request.inventory, e.request.operation_id, e.state
                        )
                    })
                    .collect();
                if records.is_empty() {
                    "none".into()
                } else {
                    records.join("\n")
                }
            }
        };
        let classification = participation::read(&state).map_err(|e| e.to_string())?;
        let held = classification.participation();
        return Ok(format!(
            "Shared apply: {}\nAgreed revision: {}\nApplied revision: {}\nUnresolved: {}\nIncomplete: {}\nManagement operation: {}\nRecovery backup: {}\n/sync apply · /sync recover <operation-id>",
            if held.is_some() {
                "participating"
            } else {
                "not participating"
            },
            held.as_ref()
                .and_then(|p| p.agreed_commit.as_deref())
                .unwrap_or("none"),
            held.as_ref()
                .and_then(|p| p.applied_commit.as_deref())
                .unwrap_or("none"),
            held.as_ref().map_or(0, |p| p.unresolved.len()),
            pending,
            management_pending,
            recovery,
        ));
    }
    if recovering {
        let active = active.ok_or("No incomplete shared apply")?;
        let key = active.descriptor().operation_key.as_hex();
        if active.descriptor().inventory_id != ApplyInventoryV1::C12 || words[1] != key {
            return Err(
                "Specify the displayed incomplete C12 operation ID. No changes were made.".into(),
            );
        }
        let parent = owner
            .c12_recovery_parent(false)
            .map_err(|e| e.to_string())?;
        let view = render::SyncConfirmView {
            recovering: true,
            project: project.display().to_string(),
            operation: key.clone(),
            revision: active.descriptor().agreed_revision.value.clone(),
            previous_revision: "no change".into(),
            recovery,
            changes: Vec::new(),
        };
        if !confirm(&view) {
            return Ok("Shared apply recovery cancelled".into());
        }
        return match owner.recover_current(&parent).map_err(|e| e.to_string())? {
            ApplyOwnerRecoveryV1::OutcomeUnknown(_) => Ok(format!(
                "Apply outcome is unknown ({key}). Source files were not reapplied. Recovery backup: {}",
                view.recovery
            )),
            ApplyOwnerRecoveryV1::Archived(_) => Ok(format!(
                "Closed the operation using saved evidence ({key}). Source files were not reapplied. Inspect state with /sync."
            )),
            ApplyOwnerRecoveryV1::Idle => {
                Err("Incomplete state changed. Inspect it with /sync.".into())
            }
        };
    }
    if let Some(active) = active {
        return Err(format!(
            "An incomplete apply exists ({}). Inspect it with /sync. No new apply was performed.",
            active.descriptor().operation_key.as_hex()
        ));
    }
    owner.ensure_sync_idle().map_err(|e| e.to_string())?;
    let mut held = participation::read(&state)
        .map_err(|e| e.to_string())?
        .participation()
        .cloned()
        .ok_or("This project has not joined a share")?;
    if !held.unresolved.is_empty() {
        return Err(format!(
            "{} unresolved items. Resolve them first.",
            held.unresolved.len()
        ));
    }
    let agreed = held
        .agreed_commit
        .clone()
        .ok_or("No agreed content. Run polaris sync pull first.")?;
    if held.applied_commit.as_deref() == Some(&agreed) {
        return Ok("This content has already been applied.".into());
    }
    if std::fs::metadata(&project)
        .map_err(|e| e.to_string())?
        .dev()
        != std::fs::metadata(&state).map_err(|e| e.to_string())?.dev()
    {
        return Err("A recovery location on the same filesystem as the project is required".into());
    }
    let parent = owner.c12_recovery_parent(true).map_err(|e| e.to_string())?;
    let paths = SyncPaths::under(&state);
    let staging = paths.work_tree.clone();
    let repository = SyncRepository::open(paths).map_err(|e| e.to_string())?;
    if repository.head().as_deref() != Some(&agreed) {
        return Err(
            "Sync records do not match the agreed content. Run polaris sync pull first.".into(),
        );
    }
    repository.restore(&agreed).map_err(|e| e.to_string())?;
    let (written, removed) = repository
        .changed_paths(held.applied_commit.as_deref(), &agreed)
        .map_err(|e| e.to_string())?;
    let limits = Limits {
        max_entries: 40_000,
        max_files: 20_000,
        max_file_bytes: 16 * 1024 * 1024,
        max_total_bytes: 256 * 1024 * 1024,
        max_depth: 32,
    };
    let (snapshot, changes) = polaris_auth::protection::with_protected_paths(|registered| {
        let secrets =
            RegisteredSecrets::from_auth_registry(registered).map_err(|e| e.to_string())?;
        let (snapshot, shaped) = apply::shape_copy(
            &project,
            &staging,
            &held.manifest,
            &apply::Changed { written, removed },
            &secrets,
            limits,
        )
        .map_err(|e| e.to_string())?;
        if !shaped.deferred.is_empty() {
            return Err("Shared changes requiring new directories remain".to_string());
        }
        let changes =
            apply::planned_changes(&snapshot, &secrets, limits).map_err(|e| e.to_string())?;
        if !changes.is_applicable() {
            return Err("Cannot apply shared content containing protected resources".into());
        }
        Ok::<_, String>((snapshot, changes))
    })
    .map_err(|_| "Cannot verify protection state".to_string())??;
    let revision = |value: &str| -> Result<RevisionV1, String> {
        let kind = match value.len() {
            40 => RevisionKindV1::GitSha1,
            64 => RevisionKindV1::GitSha256,
            _ => return Err("Cannot verify the agreed revision format".into()),
        };
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Cannot verify the agreed revision format".into());
        }
        Ok(RevisionV1 {
            kind,
            value: value.into(),
        })
    };
    let agreed_revision = revision(&agreed)?;
    let previous_applied_revision = held.applied_commit.as_deref().map(revision).transpose()?;
    let management_operation_id =
        polaris_core::management_effect::ManagementOperationIdV1::random()
            .map_err(|e| e.to_string())?;
    let view = render::SyncConfirmView {
        recovering: false,
        project: project.display().to_string(),
        operation: management_operation_id.to_string(),
        revision: agreed.clone(),
        previous_revision: held.applied_commit.clone().unwrap_or_else(|| "none".into()),
        recovery,
        changes: changes
            .changes
            .iter()
            .map(|c| format!("{:?}: {}", c.kind, c.relative_path.display()))
            .collect(),
    };
    if !confirm(&view) {
        return Ok("Shared content apply cancelled".into());
    }
    held.applied_commit = Some(agreed);
    let admission = C12ApplyAdmissionV1 {
        management_operation_id,
        agreed_revision: agreed_revision.clone(),
        staged_revision: agreed_revision,
        previous_applied_revision,
        target_participation: owner
            .participation_target_bytes(&held)
            .map_err(|e| e.to_string())?,
    };
    // Apply precisely the inspected snapshot; do not reconstruct it after y.
    let report = owner
        .apply_c12(admission, &snapshot, &changes, limits, &parent)
        .map_err(|e| format!("Cannot finalize the apply. Inspect state with /sync: {e}"))?;
    if let Some(failure) = report.failure {
        return Err(format!(
            "Apply outcome is unknown: {failure:?}. Inspect state with /sync."
        ));
    }
    Ok(format!(
        "Applied {} items. Recovery backup: {}",
        report.entries.len(),
        view.recovery
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn confirm_sync<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    view: &render::SyncConfirmView,
) -> bool {
    if view.operation.starts_with("C08 ")
        || view.operation.starts_with("C11 ")
        || view.operation.starts_with("C13 ")
    {
        return account::confirm(
            terminal,
            reader,
            &view.operation,
            &view.project,
            &view.revision,
        );
    }
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_sync_confirmation(f, view))
            .is_err()
        {
            return false;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Char('y')) => return true,
            Ok(KeyCode::Esc) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn handle_sync_action<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    project: &Path,
    state: &Path,
    requested: &str,
    has_queued_input: bool,
    session: &polaris_core::session::Session,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    let result = if !requested.is_empty() && requested != "status" {
        session.check_persistence().map_err(|e| e.to_string())
    } else {
        Ok(())
    }
    .and_then(|()| {
        if is_c10_sync_request(requested) {
            c10_tui_invocation(requested)?;
            return Err("C10 operations require a pending job owned by main".into());
        }
        sync_action(project, state, requested, has_queued_input, |view| {
            confirm_sync(terminal, reader, view)
        })
    });
    match result {
        Ok(text) => {
            local_lines.extend(
                text.lines()
                    .map(|line| ratatui::text::Line::from(render::sanitize(line))),
            );
            *status = Status::Notice("Shared apply state displayed".into());
        }
        Err(error) => *status = Status::Error(render::sanitize(&error)),
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
async fn handle_source_apply_action<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    args: &RunArgs<'_>,
    audit: Arc<tokio::sync::Mutex<AuditLog>>,
    frontend: &mut Result<source_apply::Frontend, String>,
    requested: &str,
    queued: bool,
    session: &polaris_core::session::Session,
    status: &mut Status,
    lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    let mutating = requested.starts_with("run ") || requested.starts_with("resolve ");
    if mutating && (queued || session.check_persistence().is_err()) {
        *status = Status::Error("Resolve queued input or storage errors before using D27".into());
        return;
    }
    if frontend.is_err() {
        *frontend = source_apply::Frontend::open(args, audit);
    }
    let result = match frontend {
        Ok(owner) => owner.action(requested, terminal, reader).await,
        Err(error) => Err(error.clone()),
    };
    match result {
        Ok(text) => {
            lines.extend(
                text.lines()
                    .map(|line| ratatui::text::Line::from(render::sanitize(line))),
            );
            *status = Status::Notice("D27 state displayed".into());
        }
        Err(error) => *status = Status::Error(render::sanitize(&error)),
    }
}

fn confirm_workflow_change<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    view: &render::WorkflowChangeConfirmView<'_>,
) -> bool {
    loop {
        if terminal
            .borrow_mut()
            .draw(|frame| render::render_workflow_change_confirmation(frame, view))
            .is_err()
        {
            return false;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Char('y')) => return true,
            Ok(KeyCode::Esc) | Err(_) => return false,
            Ok(_) => continue,
        }
    }
}

#[cfg(unix)]
fn workflow_mode_name(config: &polaris_core::workflow::WorkflowConfig) -> &'static str {
    if !config.enabled {
        "off"
    } else {
        match config.mode {
            polaris_core::workflow::WorkflowMode::Manual => "manual",
            polaris_core::workflow::WorkflowMode::Adaptive => "adaptive",
        }
    }
}

#[cfg(unix)]
fn workflow_override_candidate(
    session: &polaris_core::session::Session,
    requested: &polaris_core::workflow::WorkflowConfig,
    skills: &[Skill],
) -> Result<polaris_core::workflow::SessionWorkflow, String> {
    let current = session
        .workflow
        .as_ref()
        .ok_or("No saved workflow configuration")?;
    let mut config = current.config.clone();
    config.enabled = requested.enabled;
    config.mode = requested.mode;
    let candidate = polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
        config,
        current.gates.clone(),
        current.state(),
        current.artifact_graph().clone(),
    )?;
    if candidate.config.enabled {
        drop(candidate.begin_turn_from_disk(skills)?);
    }
    Ok(candidate)
}

#[cfg(unix)]
enum WorkflowChangeResult {
    NoChange,
    Cancelled,
    Changed,
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn apply_confirmed_workflow_change(
    session: &mut polaris_core::session::Session,
    candidate: polaris_core::workflow::SessionWorkflow,
    inventory: polaris_core::management_effect::workflow_change::WorkflowEffectInventoryV1,
    workspace: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    state_dir: &Path,
    confirm: impl FnOnce(&render::WorkflowChangeConfirmView<'_>) -> bool,
) -> Result<WorkflowChangeResult, String> {
    use polaris_core::management_effect::workflow_change::{
        WorkflowChangeError, WorkflowChangeOwner,
    };

    session
        .check_persistence()
        .map_err(|error| error.to_string())?;
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("No conversation to store workflow settings")?;
    let preview = match WorkflowChangeOwner::preview(persistence, inventory, &candidate) {
        Ok(preview) => preview,
        Err(WorkflowChangeError::NoChange) => return Ok(WorkflowChangeResult::NoChange),
        Err(error) => return Err(error.to_string()),
    };
    let before_config = preview
        .before_config()
        .ok_or("Missing workflow configuration before the change")?;
    let candidate_config = preview
        .candidate_config()
        .ok_or("Missing workflow configuration after the change")?;
    let before_phase = preview.before_workflow().phase.to_string();
    let after_phase = preview.candidate_workflow().phase.to_string();
    let view = render::WorkflowChangeConfirmView {
        inventory: inventory.id(),
        project_id: preview.project_id(),
        session_id: preview.session_id(),
        generation: preview.before_generation(),
        before_phase: &before_phase,
        after_phase: &after_phase,
        before_mode: workflow_mode_name(&before_config.config),
        after_mode: workflow_mode_name(&candidate_config.config),
    };
    if !confirm(&view) {
        return Ok(WorkflowChangeResult::Cancelled);
    }
    #[cfg(target_os = "linux")]
    if signal::pending_linux_process_shutdown().is_some() {
        return Ok(WorkflowChangeResult::Cancelled);
    }
    let lease = workspace
        .try_mutation()
        .map_err(|error| error.to_string())?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|error| error.to_string())?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        state_dir,
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let owner = WorkflowChangeOwner {
        effects,
        lease: &lease,
        state_dir,
    };
    let operation_id = polaris_core::management_effect::ManagementOperationIdV1::random()
        .map_err(|error| error.to_string())?;
    let prepared = owner
        .prepare_confirmed(persistence, &preview, operation_id)
        .map_err(|error| error.to_string())?;
    owner
        .execute(persistence, &prepared)
        .map_err(|error| error.to_string())?;
    session.workflow = Some(candidate);
    Ok(WorkflowChangeResult::Changed)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn handle_managed_workflow_action<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    action: slash::Action,
    session: &mut polaris_core::session::Session,
    skills: &[Skill],
    workspace: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    state_dir: &Path,
    has_queued_input: bool,
    status: &mut Status,
) {
    use polaris_core::management_effect::workflow_change::WorkflowEffectInventoryV1;

    if has_queued_input {
        *status = Status::Notice("Clear queued input before changing workflow settings".into());
        return;
    }
    let result = (|| -> Result<Option<String>, String> {
        session
            .check_persistence()
            .map_err(|error| error.to_string())?;
        let current = session
            .workflow
            .as_ref()
            .ok_or("Workflow is disabled for this session")?;
        let (inventory, candidate, notice) = match action {
            slash::Action::Phase(requested) => {
                if requested.is_empty() {
                    return Ok(Some(format!("Workflow phase: {}", current.state().phase)));
                }
                if !current.config.enabled {
                    return Err(
                        "Cannot change phase in a conversation with workflow disabled".into(),
                    );
                }
                let phase = requested.parse::<polaris_core::workflow::Phase>()?;
                let candidate = polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
                    current.config.clone(),
                    current.gates.clone(),
                    current.state(),
                    current.artifact_graph().clone(),
                )?;
                candidate.request_phase(phase)?;
                (
                    WorkflowEffectInventoryV1::T03,
                    candidate,
                    format!("Workflow phase: {phase}"),
                )
            }
            slash::Action::Workflow(requested) => {
                if requested.is_empty() {
                    return Ok(Some(format!(
                        "workflow: {}",
                        workflow_mode_name(&current.config)
                    )));
                }
                let mut config = current.config.clone();
                match requested.strip_prefix("mode ").unwrap_or(&requested) {
                    "manual" => {
                        config.enabled = true;
                        config.mode = polaris_core::workflow::WorkflowMode::Manual;
                    }
                    "adaptive" => {
                        config.enabled = true;
                        config.mode = polaris_core::workflow::WorkflowMode::Adaptive;
                    }
                    "off" => {
                        config.enabled = false;
                        config.mode = polaris_core::workflow::WorkflowMode::Manual;
                    }
                    _ => return Err("Workflow mode must be manual, adaptive, or off".into()),
                }
                let candidate = polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
                    config,
                    current.gates.clone(),
                    current.state(),
                    current.artifact_graph().clone(),
                )?;
                let notice = format!("workflow: {}", workflow_mode_name(&candidate.config));
                (WorkflowEffectInventoryV1::T04, candidate, notice)
            }
            _ => return Err("Invalid workflow operation kind".into()),
        };
        if candidate.config.enabled {
            drop(candidate.begin_turn_from_disk(skills)?);
        }
        match apply_confirmed_workflow_change(
            session,
            candidate,
            inventory,
            workspace,
            effects,
            state_dir,
            |view| confirm_workflow_change(terminal, reader, view),
        )? {
            WorkflowChangeResult::Cancelled => Ok(None),
            WorkflowChangeResult::NoChange | WorkflowChangeResult::Changed => Ok(Some(notice)),
        }
    })();
    *status = match result {
        Ok(Some(notice)) => Status::Notice(notice),
        Ok(None) => Status::Notice("Workflow change cancelled".into()),
        Err(error) => Status::Error(format!("Cannot save workflow: {error}")),
    };
}

#[cfg(unix)]
fn select_rewind_boundary<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    session: &polaris_core::session::Session,
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    unavailable_reason: &str,
) -> Result<
    Option<(
        polaris_core::conversation_state::RewindBoundaryV1,
        polaris_core::rewind_workspace::ManifestId,
        polaris_core::rewind_workspace::ManifestId,
    )>,
    String,
> {
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("This conversation has no rewindable saved history")?;
    let boundaries = persistence
        .list_rewind_boundaries()
        .map_err(|error| error.to_string())?;
    let rows = boundaries
        .iter()
        .map(|boundary| render::RewindPickerRow {
            boundary_id: boundary.boundary_id.clone(),
            user_message: boundary
                .user_message_preview
                .clone()
                .unwrap_or_else(|| "(message unavailable)".into()),
            recorded_at_millis: boundary.created_at_unix_ms.into(),
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        run_rewind_picker(terminal, reader, &rows, unavailable_reason);
        return Err(format!(
            "No verified rewind boundaries: {unavailable_reason}"
        ));
    }
    let Some(selected_id) = run_rewind_picker(terminal, reader, &rows, unavailable_reason) else {
        return Ok(None);
    };
    let selected = boundaries
        .iter()
        .find(|boundary| boundary.boundary_id == selected_id)
        .ok_or("The selected rewind boundary was not found")?
        .clone();
    let latest = boundaries.last().ok_or("No rewind boundary")?;
    let expected_post: polaris_core::rewind_workspace::ManifestId = latest
        .completion
        .as_ref()
        .ok_or("The latest rewind boundary is incomplete")?
        .workspace_post_manifest_id
        .parse()
        .map_err(|error: polaris_core::rewind_workspace::IdParseError| error.to_string())?;
    let target_pre: polaris_core::rewind_workspace::ManifestId = selected
        .workspace_pre_manifest_id
        .parse()
        .map_err(|error: polaris_core::rewind_workspace::IdParseError| error.to_string())?;
    let preview = {
        let lease = owner.try_mutation().map_err(|error| error.to_string())?;
        owner
            .preview_with_management_with_lease(&lease, &expected_post, &target_pre)
            .map_err(|error| error.to_string())?
    };
    if !preview.restorable {
        return Err(format!(
            "Cannot restore files: {:?}",
            preview.blocked_reason
        ));
    }
    let view = render::RewindPreviewView {
        user_message: selected
            .user_message_preview
            .clone()
            .unwrap_or_else(|| "(message unavailable)".into()),
        changed_paths: preview
            .changes
            .iter()
            .map(|change| format!("{:?} {}", change.kind, change.relative_path.display()))
            .collect(),
        recovery_location: preview.recovery_root.display().to_string(),
    };
    if !confirm_rewind_preview(terminal, reader, &view) {
        return Ok(None);
    }
    Ok(Some((selected, expected_post, target_pre)))
}

#[cfg(unix)]
async fn build_rewound_session(
    source: &polaris_core::session::Session,
    branch: polaris_core::session_store::PersistedSession,
    args: &RunArgs<'_>,
    skills: &[Skill],
) -> Result<(polaris_core::session::Session, PathBuf), String> {
    let id = branch
        .snapshot()
        .map_err(|error| error.to_string())?
        .state
        .session_id;
    let path = args.sessions_dir.join(format!("{id}.jsonl"));
    let mut candidate = polaris_core::session::Session::new();
    candidate.disable_files_md_auto_regenerate = source.disable_files_md_auto_regenerate;
    candidate.input_preparation = source.input_preparation;
    candidate.strict_history = source.strict_history.clone();
    candidate.summary_usage = source.summary_usage.clone();
    candidate.examples = source.examples.clone();
    restore_prepared_continuation(
        &mut candidate,
        &PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence: branch,
        },
        &args.workflow_config,
        // A rewind branch keeps the selected checkpoint's workflow state.
        // A launch override applies only to the session opened at startup.
        false,
        skills,
        args.history_backend_factory.as_ref(),
    )
    .await?;
    #[cfg(target_os = "linux")]
    if signal::pending_linux_process_shutdown().is_some() {
        return Err(LINUX_SHUTDOWN_DURING_RESTORE.into());
    }
    candidate
        .recover_history()
        .await
        .map_err(|error| format!("Cannot restore history at the rewind destination: {error}"))?;
    #[cfg(target_os = "linux")]
    if signal::pending_linux_process_shutdown().is_some() {
        return Err(LINUX_SHUTDOWN_DURING_RESTORE.into());
    }
    configure_saved_tool_memory(
        &mut candidate,
        &args.cwd,
        &args.state_dir,
        &path,
        args.tool_memory,
        args.tool_memory_embedding.clone(),
    )
    .map_err(|error| format!("Cannot restore tool memory at the rewind destination: {error}"))?;
    if args.remember {
        memory::configure_saved_memory(&mut candidate, &args.cwd, &args.state_dir, &path)
            .map_err(|error| format!("Cannot restore memory at the rewind destination: {error}"))?;
    }
    Ok((candidate, path))
}

#[cfg(unix)]
type RewoundProjectContext = (
    Vec<Skill>,
    Vec<polaris_skills::AgentType>,
    polaris_core::mcp::local::LocalHost,
    Arc<polaris_core::mcp::setup::SetupTool>,
);

#[cfg(unix)]
fn reload_rewound_project_context(
    cwd: &Path,
    mcp_ceiling: polaris_core::mcp::operation::ExternalCeiling,
) -> Result<RewoundProjectContext, String> {
    let project = polaris_core::project::resolve_root(cwd);
    let config = polaris_core::config::load(&project)
        .map_err(|error| format!("Cannot reload settings after restoration: {error}"))?;
    let skills = polaris_skills::discover(&project, &config.skills_paths).skills;
    let agents = polaris_skills::discover_agent_types(&project, &config.agents_paths).agent_types;
    let host = polaris_core::mcp::local::LocalHost::open(cwd, false)
        .map_err(|error| format!("Cannot reload MCP settings after restoration: {error}"))?;
    let setup = Arc::new(host.setup_tool(mcp_ceiling));
    Ok((skills, agents, host, setup))
}

#[cfg(unix)]
struct RewindActivation {
    session: polaris_core::session::Session,
    session_path: PathBuf,
    skills: Vec<Skill>,
    agent_types: Vec<polaris_skills::AgentType>,
    mcp_host: polaris_core::mcp::local::LocalHost,
    mcp_setup: Arc<polaris_core::mcp::setup::SetupTool>,
}

#[cfg(unix)]
struct RewindFailure {
    message: String,
    fatal: bool,
}

#[cfg(unix)]
fn ensure_rewind_unblocked(
    project: &Path,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
) -> Result<(), String> {
    rewind_composite_result(
        polaris_core::sync::composite::C10CompositeBlockerV1::inspect_readonly(
            project,
            effects.state_dir(),
        ),
    )
}
#[cfg(unix)]
fn ensure_rewind_unblocked_with_lease(
    project: &Path,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    lease: &polaris_core::project_mutation::ProjectMutationLeaseV1,
) -> Result<(), String> {
    rewind_composite_result(
        polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_mutation(
            project,
            effects.state_dir(),
            lease,
        ),
    )
}
#[cfg(unix)]
fn rewind_composite_result(
    result: Result<
        Option<polaris_core::management_effect::ManagementEffectSnapshotV1>,
        polaris_core::sync::composite::C10CompositeErrorV1,
    >,
) -> Result<(), String> {
    match result {
        Ok(Some(effect)) => Err(format!(
            "Cannot rewind due to unresolved management operation {} ({})",
            effect.request.operation_id, effect.request.inventory
        )),
        Err(error) => Err(format!("Cannot verify management operation state: {error}")),
        Ok(None) => Ok(()),
    }
}

#[cfg(unix)]
fn read_rewind_transaction(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    lease: &polaris_core::project_mutation::ProjectMutationLeaseV1,
    transaction_id: &polaris_core::rewind_workspace::TransactionId,
    branch_id: &str,
    expected_post: &polaris_core::rewind_workspace::ManifestId,
    target_pre: &polaris_core::rewind_workspace::ManifestId,
) -> Result<polaris_core::rewind_workspace::RewindTransactionReadback, String> {
    let readback = owner
        .transaction_readback_with_lease(lease, transaction_id)
        .map_err(|error| format!("Cannot sync and reload the rewind transaction: {error}"))?;
    if readback.candidate_session_id.as_deref() != Some(branch_id)
        || readback.expected_post != *expected_post
        || readback.target_pre != *target_pre
    {
        return Err("Rewind transaction and child conversation evidence mismatch".into());
    }
    Ok(readback)
}

#[cfg(unix)]
fn rollback_known_rewind(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    lease: &polaris_core::project_mutation::ProjectMutationLeaseV1,
    transaction_id: &polaris_core::rewind_workspace::TransactionId,
    branch_id: &str,
    expected_post: &polaris_core::rewind_workspace::ManifestId,
    target_pre: &polaris_core::rewind_workspace::ManifestId,
    before: &polaris_core::rewind_workspace::RewindTransactionReadback,
) -> Result<(), String> {
    use polaris_core::rewind_workspace::RewindTransactionPhase;

    if before.phase != RewindTransactionPhase::PendingActivation {
        return Err("The rewind transaction cannot be safely rolled back".into());
    }
    let restored = before
        .restored_manifest_id
        .ok_or("Missing post-rewind manifest")?;
    if !owner
        .matches_manifest(&restored)
        .map_err(|error| error.to_string())?
    {
        return Err("Post-rewind workspace files do not match a known state".into());
    }
    let write = owner.rollback_with_lease(lease, transaction_id);
    let after = read_rewind_transaction(
        owner,
        lease,
        transaction_id,
        branch_id,
        expected_post,
        target_pre,
    )?;
    if after.phase == RewindTransactionPhase::RolledBack {
        Ok(())
    } else {
        Err(format!(
            "Cannot verify the rollback result: {}",
            write
                .err()
                .map_or_else(|| "phase mismatch".into(), |error| error.to_string())
        ))
    }
}

#[cfg(unix)]
fn commit_rewind_if_unblocked(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    transaction_id: &polaris_core::rewind_workspace::TransactionId,
    branch_id: &str,
    expected_post: &polaris_core::rewind_workspace::ManifestId,
    target_pre: &polaris_core::rewind_workspace::ManifestId,
) -> Result<(), RewindFailure> {
    use polaris_core::rewind_workspace::RewindTransactionPhase;

    let lease = owner.try_mutation().map_err(|error| RewindFailure {
        message: format!("Cannot acquire the project lease for rewind finalization: {error}"),
        fatal: true,
    })?;
    if let Err(blocker) = ensure_rewind_unblocked_with_lease(owner.project_root(), effects, &lease)
    {
        let result = read_rewind_transaction(
            owner,
            &lease,
            transaction_id,
            branch_id,
            expected_post,
            target_pre,
        )
        .and_then(|before| {
            rollback_known_rewind(
                owner,
                &lease,
                transaction_id,
                branch_id,
                expected_post,
                target_pre,
                &before,
            )
        });
        return Err(match result {
            Ok(()) => RewindFailure {
                message: blocker,
                fatal: false,
            },
            Err(recovery) => RewindFailure {
                message: format!(
                    "Cannot recover before rewind finalization: {blocker}; {recovery}"
                ),
                fatal: true,
            },
        });
    }
    let write = owner.commit_with_lease(&lease, transaction_id);
    let readback = read_rewind_transaction(
        owner,
        &lease,
        transaction_id,
        branch_id,
        expected_post,
        target_pre,
    )
    .map_err(|error| RewindFailure {
        message: format!("Rewind finalization outcome is unknown: {error}"),
        fatal: true,
    })?;
    match readback.phase {
        RewindTransactionPhase::Committed => Ok(()),
        RewindTransactionPhase::PendingActivation => {
            let reason = write.err().map_or_else(
                || "Post-finalization phase is nonterminal".to_string(),
                |error| format!("Rewind finalization failed: {error}"),
            );
            match rollback_known_rewind(
                owner,
                &lease,
                transaction_id,
                branch_id,
                expected_post,
                target_pre,
                &readback,
            ) {
                Ok(()) => Err(RewindFailure {
                    message: reason,
                    fatal: false,
                }),
                Err(recovery) => Err(RewindFailure {
                    message: format!("{reason}; cannot recover: {recovery}"),
                    fatal: true,
                }),
            }
        }
        _ => Err(RewindFailure {
            message: "Cannot verify the rewind finalization phase. Restart to recover".into(),
            fatal: true,
        }),
    }
}

#[cfg(unix)]
async fn perform_rewind(
    source: &polaris_core::session::Session,
    boundary: &polaris_core::conversation_state::RewindBoundaryV1,
    (expected_post, target_pre): (
        &polaris_core::rewind_workspace::ManifestId,
        &polaris_core::rewind_workspace::ManifestId,
    ),
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    args: &RunArgs<'_>,
    mcp_ceiling: polaris_core::mcp::operation::ExternalCeiling,
) -> Result<RewindActivation, RewindFailure> {
    ensure_rewind_unblocked(&polaris_core::project::resolve_root(&args.cwd), effects).map_err(
        |message| RewindFailure {
            message,
            fatal: false,
        },
    )?;
    let persistence = source.persistence.as_ref().ok_or_else(|| RewindFailure {
        message: "No saved session for the rewind source".into(),
        fatal: false,
    })?;
    let branch = persistence
        .prepare_rewind_branch(owner, &boundary.boundary_id)
        .map_err(|error| RewindFailure {
            message: format!("Cannot prepare the conversation fork: {error}"),
            fatal: false,
        })?;
    let branch_id = branch
        .snapshot()
        .map_err(|error| RewindFailure {
            message: format!("Cannot verify the conversation fork: {error}"),
            fatal: false,
        })?
        .state
        .session_id;
    let restored = match owner.restore_for_branch(expected_post, target_pre, &branch_id) {
        Ok(restored) => restored,
        Err(error) => {
            return match owner.resolve_pending() {
                Ok(_) => Err(RewindFailure {
                    message: format!("Cannot restore files: {error}"),
                    fatal: false,
                }),
                Err(recovery) => Err(RewindFailure {
                    message: format!(
                        "File restoration failed and the intermediate state cannot be resolved: {error}; {recovery}"
                    ),
                    fatal: true,
                }),
            };
        }
    };
    let candidate = async {
        args.always_on
            .system_for_request()
            .map_err(|error| format!("Cannot reload instructions after restoration: {error}"))?;
        let (skills, agent_types, mcp_host, mcp_setup) =
            reload_rewound_project_context(&args.cwd, mcp_ceiling)?;
        let (session, session_path) = build_rewound_session(source, branch, args, &skills).await?;
        Ok::<_, String>(RewindActivation {
            session,
            session_path,
            skills,
            agent_types,
            mcp_host,
            mcp_setup,
        })
    }
    .await;
    #[cfg(target_os = "linux")]
    let candidate = if candidate.is_ok() && signal::pending_linux_process_shutdown().is_some() {
        Err(LINUX_SHUTDOWN_DURING_RESTORE.into())
    } else {
        candidate
    };
    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(error) => {
            let recovery = (|| -> Result<(), String> {
                let lease = owner.try_mutation().map_err(|error| error.to_string())?;
                let before = read_rewind_transaction(
                    owner,
                    &lease,
                    &restored.transaction_id,
                    &branch_id,
                    expected_post,
                    target_pre,
                )?;
                rollback_known_rewind(
                    owner,
                    &lease,
                    &restored.transaction_id,
                    &branch_id,
                    expected_post,
                    target_pre,
                    &before,
                )
            })();
            return match recovery {
                Ok(()) => Err(RewindFailure {
                    message: format!("Cannot prepare the rewind destination: {error}"),
                    fatal: false,
                }),
                Err(recovery) => Err(RewindFailure {
                    message: format!(
                        "Rewind destination preparation failed and original files cannot be recovered: {error}; {recovery}"
                    ),
                    fatal: true,
                }),
            };
        }
    };
    commit_rewind_if_unblocked(
        owner,
        effects,
        &restored.transaction_id,
        &branch_id,
        expected_post,
        target_pre,
    )?;
    let committed = candidate
        .session
        .persistence
        .as_ref()
        .ok_or_else(|| RewindFailure {
            message: format!("Cannot verify storage for finalized child conversation {branch_id}"),
            fatal: true,
        })?;
    validate_resume_workspace(owner, committed).map_err(|error| RewindFailure {
        message: format!("Cannot reload finalized child conversation {branch_id}: {error}"),
        fatal: true,
    })?;
    Ok(candidate)
}

/// Handles `/resume`: shows the picker and, if the user picks a
/// conversation, swaps `session`/`session_path`/`meta_path`/
/// `session_started_at_millis` to it in place. A cancelled picker (Esc,
/// or nothing to resume) leaves every one of those untouched.
#[allow(clippy::too_many_arguments)]
async fn handle_resume<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    sessions_dir: &std::path::Path,
    state_dir: &std::path::Path,
    cwd_display: &str,
    workflow_config: &polaris_core::workflow::WorkflowConfig,
    workflow_mode_overridden: bool,
    skills: &[Skill],
    history_backend_factory: Option<&HistoryBackendFactory>,
    #[cfg(unix)] rewind_workspace: Option<&polaris_core::rewind_workspace::WorkspaceOwner>,
    #[cfg(unix)] management_effects: Option<
        &polaris_core::management_effect::ManagementEffectStoreV1,
    >,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    #[cfg(target_os = "linux")]
    let mut summaries = match sessions::try_list_sessions(sessions_dir) {
        Ok(summaries) => summaries,
        Err(error) => {
            *status = Status::Error(format!("Cannot read the saved conversation list: {error}"));
            return;
        }
    };
    #[cfg(not(target_os = "linux"))]
    let mut summaries = sessions::list_sessions(sessions_dir);
    let Some(data_root) = sessions_dir.parent() else {
        *status = Status::Notice("Invalid v2 session storage location".into());
        return;
    };
    let project_id = persist::project_identity(&polaris_core::project::resolve_root(
        std::path::Path::new(cwd_display),
    ))
    .ok();
    if let Some(project_id) = &project_id {
        #[cfg(target_os = "linux")]
        if let Some(owner) = rewind_workspace {
            match sessions::try_list_published_v2_sessions(
                owner,
                data_root,
                &state_dir.join("memory.sqlite3"),
                project_id,
                cwd_display,
            ) {
                Ok(v2) => {
                    summaries.extend(v2);
                    if summaries.len() > 256 {
                        *status = Status::Error("The conversation list exceeds 256 entries".into());
                        return;
                    }
                }
                Err(error) => {
                    *status = Status::Error(format!(
                        "Cannot read the saved v2 conversation list: {error}"
                    ));
                    return;
                }
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        if let Some(owner) = rewind_workspace {
            summaries.extend(sessions::list_v2_sessions_filtered(
                data_root,
                &state_dir.join("memory.sqlite3"),
                project_id,
                cwd_display,
                |persisted| {
                    polaris_core::rewind_admission::published_branch(owner, &persisted.store)
                        .is_ok()
                },
            ));
        }
        #[cfg(not(unix))]
        summaries.extend(sessions::list_v2_sessions(
            data_root,
            &state_dir.join("memory.sqlite3"),
            project_id,
            cwd_display,
        ));
    }
    let groups = sessions::grouped(summaries, cwd_display);
    let Some(selected) = run_resume_picker(terminal, reader, &groups) else {
        return;
    };
    #[cfg(target_os = "linux")]
    if signal::pending_linux_process_shutdown().is_some() {
        return;
    }

    if selected.kind == sessions::SessionKind::V2 {
        #[cfg(unix)]
        if rewind_workspace.is_none() {
            *status = Status::Notice(
                "Cannot resume the conversation without a workspace file verification owner".into(),
            );
            return;
        }
        let Some(project_id) = project_id.as_deref() else {
            *status =
                Status::Notice("Cannot resume the v2 session without a project identifier".into());
            return;
        };
        let strict_history = session.strict_history.clone();
        let summary_usage = session.summary_usage.clone();
        let examples = session.examples.clone();
        let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
        #[cfg(target_os = "linux")]
        let mut compatibility_published = false;
        let result = async {
            let persistence = polaris_core::session_store::PersistedSession::open(
                data_root,
                &state_dir.join("memory.sqlite3"),
                project_id,
                &selected.id,
            )
            .map_err(|error| error.to_string())?;
            #[cfg(unix)]
            persistence
                .abandon_unfinished_rewind_boundaries("The previous execution exited before completion".into())
                .map_err(|error| error.to_string())?;
            #[cfg(unix)]
            if let Some(owner) = rewind_workspace
                && let Err(error) = validate_resume_workspace(owner, &persistence)
            {
                let source = persistence.store.directory().join("raw.jsonl");
                let snapshot = persistence.snapshot().map_err(|error| error.to_string())?;
                let messages = snapshot
                    .events
                    .into_iter()
                    .map(|event| event.message)
                    .collect::<Vec<_>>();
                show_read_only_transcript(
                    terminal,
                    reader,
                    &messages,
                    &source.display().to_string(),
                );
                return Err(format!("{error}. The conversation is view-only"));
            }
            #[cfg(target_os="linux")]
            let mut frozen_resume=resume_effect::ResumeSubjectV1::Persisted(&persistence).identity()?;
            #[cfg(target_os="linux")]
            if workflow_mode_overridden
                && let Some(previous)=session.persistence.as_ref()
                && previous.store.directory()==persistence.store.directory()
                && resume_effect::ResumeSubjectV1::Persisted(previous).identity()?!=frozen_resume
            {
                return Err("The previous conversation does not match the selection".into());
            }
            let mut restored = polaris_core::session::Session::new();
            restored.disable_files_md_auto_regenerate = disable_files_md_auto_regenerate;
            restored.input_preparation = session.input_preparation;
            restored.strict_history = strict_history;
            restored.summary_usage = summary_usage;
            restored.examples = examples;
            let continuation = PreparedContinuation {
                kind: ContinuationKind::Resume,
                persistence,
            };
            if workflow_mode_overridden {
                #[cfg(unix)]
                restore_prepared_continuation_for_managed_override(
                    &mut restored,
                    &continuation,
                    workflow_config,
                    skills,
                    history_backend_factory,
                )
                .await?;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return Err("Stop request received".into());
                }
                #[cfg(not(unix))]
                return Err("A workflow change requires a management operation".into());
                #[cfg(unix)]
                {
                    restored
                        .recover_history()
                        .await
                        .map_err(|error| format!("Cannot restore strict10 history: {error}"))?;
                    #[cfg(target_os = "linux")]
                    if signal::pending_linux_process_shutdown().is_some() {
                        return Err("Stop request received".into());
                    }
                    let workspace = rewind_workspace.ok_or("No project mutation owner")?;
                    let effects = management_effects.ok_or("No management operation record")?;
                    let candidate = workflow_override_candidate(&restored, workflow_config, skills)?;
                    match apply_confirmed_workflow_change(
                        &mut restored,
                        candidate,
                        polaris_core::management_effect::workflow_change::WorkflowEffectInventoryV1::F15,
                        workspace,
                        effects,
                        state_dir,
                        |view| confirm_workflow_change(terminal, reader, view),
                    )? {
                        WorkflowChangeResult::Cancelled => {
                            return Err("Workflow change cancelled".into());
                        }
                        WorkflowChangeResult::Changed | WorkflowChangeResult::NoChange => {}
                    }
                }
            } else {
                let publication = restore_prepared_continuation_inner(
                    &mut restored,
                    &continuation,
                    workflow_config,
                    false,
                    true,
                    skills,
                    history_backend_factory,
                )
                .await?;
                #[cfg(target_os="linux")]
                if let Some(publication) = publication {
                    if resume_effect::ResumeIdentityV1::published_snapshot(&publication.before)? != frozen_resume {
                        return Err("The selected conversation changed before compatibility recovery".into());
                    }
                    frozen_resume = resume_effect::ResumeIdentityV1::published_snapshot(&publication.published)?;
                    compatibility_published = true;
                }
                #[cfg(not(target_os="linux"))]
                let _ = publication;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return Err("Stop request received".into());
                }
                restored
                    .recover_history()
                    .await
                    .map_err(|error| format!("Cannot restore strict10 history: {error}"))?;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return Err("Stop request received".into());
                }
            }
            #[cfg(target_os="linux")]
            {
                let restored_saved=restored.persistence.as_ref().ok_or("No destination conversation")?;
                if !workflow_mode_overridden && resume_effect::ResumeSubjectV1::Persisted(restored_saved).identity()?!=frozen_resume {return Err("The selected conversation changed during restoration".into());}
            }
            Ok::<_, String>(restored)
        }
        .await;
        match result {
            Ok(restored) => {
                #[cfg(target_os = "linux")]
                {
                    let result = (|| -> Result<(), String> {
                        let workspace = rewind_workspace.ok_or("No project mutation owner")?;
                        let effects = management_effects.ok_or("No management effect owner")?;
                        let target = restored
                            .persistence
                            .clone()
                            .ok_or("The selected v2 conversation does not exist")?;
                        // F15 or compatibility recovery has published this
                        // exact session. Use its verified new cursor.
                        let same_session_override = (workflow_mode_overridden
                            || compatibility_published)
                            && session.persistence.as_ref().is_some_and(|previous| {
                                previous.store.directory() == target.store.directory()
                            });
                        let previous = if same_session_override {
                            Some(target.clone())
                        } else {
                            session.persistence.clone()
                        };
                        let lease = workspace.try_mutation().map_err(|e| e.to_string())?;
                        polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
                            workspace.project_root(), effects.state_dir(), &lease,
                        ).map_err(|e| e.to_string())?;
                        let owner = resume_effect::ResumeSelectionOwnerV1 {
                            effects,
                            lease: &lease,
                        };
                        let from = previous
                            .as_ref()
                            .map(resume_effect::ResumeSubjectV1::Persisted);
                        let to = resume_effect::ResumeSubjectV1::Persisted(&target);
                        let prepared =
                            owner.prepare(resume_effect::ResumeInventoryV1::D07, from, to)?;
                        if same_session_override {
                            // The owned publication remains committed even if
                            // the later D07 selection is declined.
                            session.persistence = Some(target.clone());
                            session.workflow = restored.workflow.clone();
                        }
                        let accepted = account::confirm(
                            terminal,
                            reader,
                            "Select conversation",
                            &selected.id,
                            "Verify the selected saved conversation's workspace association and original text, then switch to it.",
                        );
                        let mut candidate = Some(restored);
                        owner.execute(&prepared, from, to, accepted, || {
                            *session = candidate.take().ok_or("No restore candidates")?;
                            Ok(())
                        })?;
                        if !accepted {
                            return Err("Conversation selection cancelled".into());
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        *status = Status::Error(error);
                        return;
                    }
                }
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                #[cfg(not(target_os = "linux"))]
                {
                    *session = restored;
                }
                *session_started_at_millis = selected.started_at_millis;
                *session_path = sessions_dir.join(format!("{}.jsonl", selected.id));
                *meta_path = session_path.with_extension("meta.json");
                local_lines.clear();
                *status = Status::Idle;
            }
            Err(error) => {
                *status = Status::Notice(format!("Cannot resume the v2 session: {error}"))
            }
        }
        return;
    }

    if workflow_mode_overridden {
        *status =
            Status::Notice("Applying --workflow-mode to a legacy conversation requires a migration management operation".into());
        return;
    }
    let new_session_path = selected.path;
    let new_meta_path = new_session_path.with_extension("meta.json");
    #[cfg(unix)]
    if let Some(owner) = rewind_workspace {
        match owner.has_workspace_identity() {
            Ok(true) => {
                match persist::load_session(&new_session_path) {
                    Ok((loaded, _)) => show_read_only_transcript(
                        terminal,
                        reader,
                        &loaded.messages,
                        &new_session_path.display().to_string(),
                    ),
                    Err(error) => {
                        *status = Status::Notice(format!(
                            "Cannot read the conversation source text: {error}"
                        ));
                        return;
                    }
                }
                *status = Status::Notice(
                    "The legacy conversation is view-only because it has no saved workspace file association"
                        .into(),
                );
                return;
            }
            Err(error) => {
                *status = Status::Notice(format!("Cannot verify workspace files: {error}"));
                return;
            }
            Ok(false) => {}
        }
    }
    match persist::load_session(&new_session_path) {
        Ok((mut loaded, truncated)) => {
            if let Some(previous) = session.persistence.clone() {
                let Some(project_id) = project_id.as_deref() else {
                    *status = Status::Notice(
                        "Cannot migrate the legacy session without a project identifier".into(),
                    );
                    return;
                };
                let strict_history = session.strict_history.clone();
                let summary_usage = session.summary_usage.clone();
                let examples = session.examples.clone();
                let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
                let workflow = session.workflow.clone();
                let mode = previous
                    .snapshot()
                    .map(|snapshot| snapshot.state.history_mode)
                    .unwrap_or(polaris_core::conversation_state::HistoryMode::Legacy);
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                match polaris_core::session_store::PersistedSession::import(
                    data_root,
                    &state_dir.join("memory.sqlite3"),
                    project_id,
                    mode,
                    workflow.as_ref(),
                    &loaded.messages,
                ) {
                    Ok(persistence) => {
                        #[cfg(target_os = "linux")]
                        if signal::pending_linux_process_shutdown().is_some() {
                            return;
                        }
                        #[cfg(unix)]
                        if let Some(owner) = rewind_workspace
                            && let Err(error) = validate_resume_workspace(owner, &persistence)
                        {
                            *status = Status::Notice(format!(
                                "Cannot bind the legacy session to current workspace files: {error}"
                            ));
                            return;
                        }
                        let mut imported = polaris_core::session::Session::new();
                        imported.input_preparation = session.input_preparation;
                        imported.disable_files_md_auto_regenerate =
                            disable_files_md_auto_regenerate;
                        imported.workflow = workflow;
                        if let Err(error) = imported.attach(persistence) {
                            *status = Status::Notice(format!(
                                "Cannot migrate the legacy session: {error}"
                            ));
                            return;
                        }
                        imported.strict_history = strict_history;
                        imported.summary_usage = summary_usage;
                        imported.examples = examples;
                        if let Err(error) = imported.recover_history().await {
                            *status =
                                Status::Notice(format!("Cannot restore strict10 history: {error}"));
                            return;
                        }
                        #[cfg(target_os = "linux")]
                        if signal::pending_linux_process_shutdown().is_some() {
                            return;
                        }
                        *session = imported;
                    }
                    Err(error) => {
                        *status =
                            Status::Notice(format!("Cannot migrate the legacy session: {error}"));
                        return;
                    }
                }
            } else {
                loaded.disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
                loaded.input_preparation = session.input_preparation;
                *session = loaded;
            }
            *session_started_at_millis = persist::read_meta(&new_meta_path)
                .map(|m| m.started_at_millis)
                .unwrap_or_else(now_millis);
            *session_path = new_session_path;
            *meta_path = new_meta_path;
            local_lines.clear();
            *status = if truncated {
                Status::Notice(format!(
                    "A corrupt line was found in {}. Resumed messages up to that point",
                    session_path.display()
                ))
            } else {
                Status::Idle
            };
        }
        Err(e) => *status = Status::Notice(format!("Cannot resume the session: {e}")),
    }
}

/// Handles `/new`: rotates to a brand-new conversation (fresh id, fresh
/// file) without touching the one being left — unlike the old
/// single-session-per-project model, it stays on disk and shows up in a
/// later `/resume`.
#[cfg(any(test, not(unix)))]
fn handle_new_session(
    sessions_dir: &std::path::Path,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) -> bool {
    handle_new_session_with(
        sessions_dir,
        session,
        session_path,
        meta_path,
        session_started_at_millis,
        status,
        local_lines,
        |previous, snapshot, workflow| {
            polaris_core::session_store::PersistedSession::create(
                &previous.data_root,
                &previous.database,
                &snapshot.state.project_id,
                snapshot.state.history_mode,
                workflow,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_new_session_with(
    sessions_dir: &std::path::Path,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
    create: impl FnOnce(
        &polaris_core::session_store::PersistedSession,
        &polaris_core::conversation_state::ConversationSnapshot,
        Option<&polaris_core::workflow::SessionWorkflow>,
    ) -> std::io::Result<polaris_core::session_store::PersistedSession>,
) -> bool {
    let id = if let Some(previous) = session.persistence.clone() {
        let strict_history = session.strict_history.clone();
        let summary_usage = session.summary_usage.clone();
        let examples = session.examples.clone();
        let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
        let input_preparation = session.input_preparation;
        let snapshot = match previous.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                *status = Status::Notice(format!("Cannot create a new v2 session: {error}"));
                return false;
            }
        };
        let mut next = polaris_core::session::Session::new();
        next.disable_files_md_auto_regenerate = disable_files_md_auto_regenerate;
        next.input_preparation = input_preparation;
        if let Some(workflow) = &session.workflow {
            next.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
                workflow.config.clone(),
            ));
        }
        let persistence = create(&previous, &snapshot, next.workflow.as_ref());
        match persistence.and_then(|persistence| {
            let id = persistence.snapshot()?.state.session_id;
            next.attach(persistence)?;
            Ok(id)
        }) {
            Ok(id) => {
                next.strict_history = strict_history;
                next.summary_usage = summary_usage;
                next.examples = examples;
                *session = next;
                id
            }
            Err(error) => {
                *status = Status::Notice(format!("Cannot create a new v2 session: {error}"));
                return false;
            }
        }
    } else {
        let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
        let input_preparation = session.input_preparation;
        *session = polaris_core::session::Session::default();
        session.disable_files_md_auto_regenerate = disable_files_md_auto_regenerate;
        session.input_preparation = input_preparation;
        new_session_id()
    };
    *session_path = sessions_dir.join(format!("{id}.jsonl"));
    *meta_path = session_path.with_extension("meta.json");
    *session_started_at_millis = now_millis();
    local_lines.clear();
    *status = Status::Idle;
    true
}

/// Handles `/fork`: like `/new`, rotates to a brand-new file/id without
/// touching the one being left, except the new one is seeded with every
/// message so far instead of starting empty — codex's own `/fork`
/// ("fork the current chat") does the same. The seeded messages are
/// written to disk immediately (unlike a fresh `/new` session, which
/// only starts existing on disk lazily at the first new message) — a
/// fork with no messages yet added since forking would otherwise vanish
/// from `/resume`'s list, which defeats the point of forking one.
#[cfg(any(test, not(unix)))]
fn handle_fork(
    sessions_dir: &std::path::Path,
    cwd_display: &str,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
) {
    handle_fork_with(
        sessions_dir,
        cwd_display,
        session,
        session_path,
        meta_path,
        session_started_at_millis,
        status,
        |previous| previous.fork(""),
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_fork_with(
    sessions_dir: &std::path::Path,
    cwd_display: &str,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    fork: impl FnOnce(
        &polaris_core::session_store::PersistedSession,
    ) -> std::io::Result<polaris_core::session_store::PersistedSession>,
) {
    if let Some(previous) = session.persistence.clone() {
        let strict_history = session.strict_history.clone();
        let summary_usage = session.summary_usage.clone();
        let examples = session.examples.clone();
        let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
        let input_preparation = session.input_preparation;
        let workflow_config = session
            .workflow
            .as_ref()
            .map(|workflow| workflow.config.clone());
        let persistence = fork(&previous);
        let mut next = polaris_core::session::Session::new();
        next.disable_files_md_auto_regenerate = disable_files_md_auto_regenerate;
        next.input_preparation = input_preparation;
        next.strict_history = strict_history;
        next.summary_usage = summary_usage;
        next.examples = examples;
        match persistence.and_then(|persistence| {
            if let Some(config) = workflow_config {
                next.workflow = Some(persistence.restore_workflow(config)?);
            }
            let id = persistence.snapshot()?.state.session_id;
            next.attach(persistence)?;
            Ok(id)
        }) {
            Ok(id) => {
                *session = next;
                *session_path = sessions_dir.join(format!("{id}.jsonl"));
                *meta_path = session_path.with_extension("meta.json");
                *session_started_at_millis = now_millis();
                *status = Status::Notice(format!("Forked into v2 session: {id}"));
            }
            Err(error) => *status = Status::Notice(format!("Cannot fork the v2 session: {error}")),
        }
        return;
    }
    let has_references = session.messages.iter().any(|message| {
        message.content.contains("memory://")
            || message.content.starts_with("[Stored tool result ")
            || message
                .tool_calls
                .iter()
                .any(|call| call.arguments.to_string().contains("memory://"))
    });
    let saved_origin = match persist::read_tool_memory_origin(meta_path) {
        Ok(origin) => origin,
        Err(error) => {
            *status = Status::Notice(format!(
                "Cannot fork because memory provenance cannot be verified: {error}"
            ));
            return;
        }
    };
    let needs_memory_origin =
        has_references || session.tool_memory.is_some() || saved_origin.is_some();
    let memory_origin =
        if memory::saved_project_matches(session, std::path::Path::new(cwd_display), session_path)
            && persist::read_meta(meta_path).is_some()
            && session_path.is_file()
        {
            match saved_tool_memory_namespace(std::path::Path::new(cwd_display), session_path) {
                Ok((id, origins)) if origins.len() < MAX_TOOL_MEMORY_ORIGINS => Some(id),
                Ok(_) => {
                    *status = Status::Notice(
                        "Cannot fork because memory lineage reached the limit".into(),
                    );
                    return;
                }
                Err(error) => {
                    *status = Status::Notice(format!(
                        "Cannot fork because memory provenance cannot be verified: {error}"
                    ));
                    return;
                }
            }
        } else {
            None
        };
    if needs_memory_origin && memory_origin.is_none() {
        *status = Status::Notice(
            "Cannot fork because memory provenance cannot be verified. The original conversation can continue unchanged.".into(),
        );
        return;
    }
    let new_session_path = sessions_dir.join(format!("{}.jsonl", new_session_id()));
    let new_meta_path = new_session_path.with_extension("meta.json");
    let started = now_millis();
    let origin_cwd = persist::read_meta(meta_path)
        .map(|meta| meta.cwd)
        .unwrap_or_else(|| {
            if session.messages.is_empty() {
                cwd_display.to_owned()
            } else {
                String::new()
            }
        });

    if let Err(e) = persist::write_fork_meta(
        &new_meta_path,
        &persist::SessionMeta {
            cwd: origin_cwd,
            started_at_millis: started,
        },
        memory_origin.as_deref(),
    ) {
        *status = Status::Notice(format!("can't fork: {e}"));
        return;
    }
    for m in &session.messages {
        if let Err(e) = persist::append_message(&new_session_path, m) {
            *status = Status::Notice(format!("can't fork: {e}"));
            return;
        }
    }

    *status = Status::Notice(if memory_origin.is_some() {
        format!(
            "Fork destination: {}. Ancestor memory is read-only; new memory is saved in this fork. Deleting ancestor memory also disables memory use in this fork.",
            new_session_path.display()
        )
    } else {
        format!("forked to {}", new_session_path.display())
    });
    *session_path = new_session_path;
    *meta_path = new_meta_path;
    *session_started_at_millis = started;
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn handle_managed_new_session(
    sessions_dir: &Path,
    state_dir: &Path,
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) -> bool {
    if session.persistence.is_none() {
        *status = Status::Notice("/new with management records requires a v2 session".into());
        return false;
    }
    let lease = match owner.try_mutation() {
        Ok(lease) => lease,
        Err(error) => {
            *status = Status::Notice(format!("Cannot start a new conversation: {error}"));
            return false;
        }
    };
    let switch = polaris_core::session_switch::SessionSwitchOwner {
        effects,
        lease: &lease,
        state_dir,
    };
    handle_new_session_with(
        sessions_dir,
        session,
        session_path,
        meta_path,
        session_started_at_millis,
        status,
        local_lines,
        |previous, snapshot, workflow| {
            switch
                .create_new(
                    Some(previous),
                    &previous.data_root,
                    &previous.database,
                    &snapshot.state.project_id,
                    snapshot.state.history_mode,
                    workflow,
                    polaris_core::management_effect::ManagementInventoryIdV1::parse("T06")
                        .expect("approved T06 inventory"),
                )
                .map_err(|error| std::io::Error::other(error.to_string()))
        },
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn handle_managed_fork(
    sessions_dir: &Path,
    cwd_display: &str,
    state_dir: &Path,
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    effects: &polaris_core::management_effect::ManagementEffectStoreV1,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
) {
    if session.persistence.is_none() {
        *status = Status::Notice("/fork with management records requires a v2 session".into());
        return;
    }
    let lease = match owner.try_mutation() {
        Ok(lease) => lease,
        Err(error) => {
            *status = Status::Notice(format!("Cannot fork the conversation: {error}"));
            return;
        }
    };
    let switch = polaris_core::session_switch::SessionSwitchOwner {
        effects,
        lease: &lease,
        state_dir,
    };
    handle_fork_with(
        sessions_dir,
        cwd_display,
        session,
        session_path,
        meta_path,
        session_started_at_millis,
        status,
        |previous| {
            switch
                .fork(
                    previous,
                    polaris_core::management_effect::ManagementInventoryIdV1::parse("T16")
                        .expect("approved T16 inventory"),
                )
                .map_err(|error| std::io::Error::other(error.to_string()))
        },
    );
}

fn approval_within_startup_ceiling(ceiling: ApprovalPolicy, picked: ApprovalPolicy) -> bool {
    // `Always` adds mandatory prompts for in-scope work, but also maps to a
    // higher child-delegation capability. Neither axis may widen at runtime.
    match ceiling {
        ApprovalPolicy::Never => picked == ApprovalPolicy::Never,
        ApprovalPolicy::OnRequest => picked != ApprovalPolicy::Always,
        ApprovalPolicy::Always => picked == ApprovalPolicy::Always,
    }
}

/// Runs the `/permissions` picker as its own small blocking loop, the
/// same shape as `run_resume_picker` (three fixed rows instead of a
/// dynamic list, so no empty-state branch is needed). Returns the picked
/// policy, or `None` if the user backed out with Esc.
fn run_permissions_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    current: ApprovalPolicy,
    ceiling: ApprovalPolicy,
    sandbox: &SandboxPolicy,
) -> Option<ApprovalPolicy> {
    const OPTIONS: [ApprovalPolicy; 3] = [
        ApprovalPolicy::Never,
        ApprovalPolicy::OnRequest,
        ApprovalPolicy::Always,
    ];
    let mut selected = OPTIONS.iter().position(|p| *p == current).unwrap_or(0);
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_permissions_picker(f, current, ceiling, sandbox, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % OPTIONS.len(),
            Ok(KeyCode::Up) => selected = (selected + OPTIONS.len() - 1) % OPTIONS.len(),
            Ok(KeyCode::Enter)
                if terminal.borrow().size().is_ok_and(|area| {
                    area.width >= render::PERMISSIONS_MIN_WIDTH
                        && area.height >= render::PERMISSIONS_MIN_HEIGHT
                }) =>
            {
                return Some(OPTIONS[selected]);
            }
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Handles `/permissions`: shows the picker and, on selection, updates
/// `approval_policy` in place — the local `run()` holds instead of
/// `args.approval_policy` directly, since `args` isn't (and shouldn't
/// become) mutable just so one slash command can override it for the
/// rest of the session.
fn handle_permissions<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    approval_policy: &mut ApprovalPolicy,
    approval_ceiling: ApprovalPolicy,
    sandbox: &SandboxPolicy,
    status: &mut Status,
) {
    if let Some(picked) = run_permissions_picker(
        terminal,
        reader,
        *approval_policy,
        approval_ceiling,
        sandbox,
    ) {
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return;
        }
        if approval_within_startup_ceiling(approval_ceiling, picked) {
            *approval_policy = picked;
            *status = Status::Notice(format!("permissions set to {picked:?}"));
        } else {
            *status = Status::Notice(format!(
                "Incompatible with the startup approval boundary. Restart with --approval {}",
                render::approval_cli_value(picked)
            ));
        }
    }
}

#[derive(Clone, Copy)]
enum ModelMenuChoice {
    Hosted(&'static str),
    Local,
}

struct ConnectedLocalModel {
    runtime: Runtime,
    adapter: Arc<LocalAdapter>,
    observed: ModelMetadata,
}

impl ConnectedLocalModel {
    fn provider_name(&self) -> &'static str {
        match self.runtime {
            Runtime::Ollama => "ollama",
            Runtime::LmStudio => "lmstudio",
            Runtime::LlamaCpp => "llamacpp",
        }
    }

    fn runtime_label(&self) -> &'static str {
        match self.runtime {
            Runtime::Ollama => "Ollama",
            Runtime::LmStudio => "LM Studio",
            Runtime::LlamaCpp => "llama.cpp",
        }
    }

    fn display_name(&self) -> String {
        format!("{} · {}", self.runtime_label(), self.observed.id())
    }
}

async fn discover_local_runtime(runtime: Runtime, endpoint: &str) -> Vec<ConnectedLocalModel> {
    let Ok(endpoint) = Endpoint::parse(endpoint) else {
        return Vec::new();
    };
    let Ok(adapter) = LocalAdapter::new(runtime, endpoint) else {
        return Vec::new();
    };
    let adapter = Arc::new(adapter);
    let Ok(models) = adapter.inventory().await else {
        return Vec::new();
    };
    models
        .into_iter()
        .filter(|model| model.capabilities().completion != Capability::Unsupported)
        .map(|observed| ConnectedLocalModel {
            runtime,
            adapter: adapter.clone(),
            observed,
        })
        .collect()
}

async fn discover_connected_local_models() -> Vec<ConnectedLocalModel> {
    let (ollama, lmstudio) = tokio::join!(
        discover_local_runtime(Runtime::Ollama, "http://127.0.0.1:11434/"),
        discover_local_runtime(Runtime::LmStudio, "http://127.0.0.1:1234/"),
    );
    ollama.into_iter().chain(lmstudio).collect()
}

/// Runs the `/model` picker — three hosted choices plus one local-runtime
/// entry. Reasoning effort is intentionally absent because routing owns it.
fn run_model_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    current: &str,
    current_is_local: bool,
) -> Option<ModelMenuChoice> {
    let catalog = render::MODEL_CATALOG;
    let mut selected = if current_is_local {
        catalog.len()
    } else {
        catalog.iter().position(|m| *m == current).unwrap_or(0)
    };
    let count = catalog.len() + 1;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_model_picker(f, current, current_is_local, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % count,
            Ok(KeyCode::Up) => selected = (selected + count - 1) % count,
            Ok(KeyCode::Enter) if selected == catalog.len() => {
                return Some(ModelMenuChoice::Local);
            }
            Ok(KeyCode::Enter) => return Some(ModelMenuChoice::Hosted(catalog[selected])),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn run_local_model_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    models: &[ConnectedLocalModel],
    current_provider: &str,
    current_model: &str,
) -> Option<usize> {
    if models.is_empty() {
        return None;
    }
    let labels: Vec<String> = models
        .iter()
        .map(ConnectedLocalModel::display_name)
        .collect();
    let current = models.iter().position(|model| {
        model.provider_name() == current_provider && model.observed.id() == current_model
    });
    let current_label = current.map(|index| labels[index].as_str());
    let mut selected = current.unwrap_or(0);
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_local_model_picker(f, &labels, current_label, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % models.len(),
            Ok(KeyCode::Up) => selected = (selected + models.len() - 1) % models.len(),
            Ok(KeyCode::Enter) => return Some(selected),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Handles `/model`. Hosted choices reuse the authenticated provider. The
/// local row discovers only the same two loopback runtimes as the desktop,
/// then installs an immutable local binding for the chosen observed model.
#[allow(clippy::too_many_arguments)]
async fn handle_model<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    provider: &mut Arc<dyn Provider>,
    cloud_provider: &Arc<dyn Provider>,
    usage_meter: &polaris_provider::UsageMeter,
    local_router: &polaris_core::local_execution::LocalRouter,
    provider_name: &mut String,
    cloud_provider_name: &str,
    model_name: &mut String,
    cloud_effort_name: &str,
    effort_name: &mut String,
    status: &mut Status,
) {
    loop {
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return;
        }
        let current_is_local = is_local_provider(provider_name);
        let Some(choice) = run_model_picker(terminal, reader, model_name, current_is_local) else {
            return;
        };
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return;
        }
        match choice {
            ModelMenuChoice::Hosted(picked_model) => {
                if !matches!(cloud_provider_name, "codex" | "openai") {
                    *status = Status::Error(
                        "hosted models are unavailable for this launch; restart with a hosted provider"
                            .to_string(),
                    );
                    return;
                }
                cloud_provider.set_model(picked_model);
                *provider = cloud_provider.clone();
                *provider_name = cloud_provider_name.to_string();
                *model_name = picked_model.to_string();
                *effort_name = cloud_effort_name.to_string();
                *status = Status::Notice(format!("model set to {picked_model}"));
                return;
            }
            ModelMenuChoice::Local => {
                let models = discover_connected_local_models().await;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                let Some(index) =
                    run_local_model_picker(terminal, reader, &models, provider_name, model_name)
                else {
                    if models.is_empty() {
                        *status = Status::Notice("no connected local models".to_string());
                        return;
                    }
                    continue;
                };
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                let picked = &models[index];
                let selection = match picked.adapter.select(&picked.observed).await {
                    Ok(selection) => selection,
                    Err(error) => {
                        *status = Status::Error(format!("local model is unavailable: {error}"));
                        return;
                    }
                };
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                if selection.model().capabilities().tools != Capability::Supported {
                    *status = Status::Error(
                        "local model does not report the tool support required by polaris"
                            .to_string(),
                    );
                    return;
                }
                let binding = match local_router.bind(selection) {
                    Ok(binding) => binding,
                    Err(error) => {
                        *status = Status::Error(format!("local model cannot be used: {error}"));
                        return;
                    }
                };
                *provider = Arc::new(usage_meter.wrap(binding));
                *provider_name = picked.provider_name().to_string();
                *model_name = picked.observed.id().to_string();
                effort_name.clear();
                *status = Status::Notice(format!(
                    "local model set to {} · {}",
                    picked.runtime_label(),
                    picked.observed.id()
                ));
                return;
            }
        }
    }
}

/// Runs compaction unconditionally (ignores the threshold — that's the
/// point of a manual command) and reports the result via `status`. On
/// success, also resyncs the persisted session log so `/resume` reflects
/// the compacted state, matching how `TurnOutcome::Done(Ok(_))` does the
/// same thing after an automatic compaction (see the call site in `run`).
async fn handle_compact(
    provider: &dyn Provider,
    session: &mut polaris_core::session::Session,
    session_path: &std::path::Path,
    status: &mut Status,
    usage: &mut polaris_provider::Usage,
) -> bool {
    let outcome = polaris_core::compaction::compact_with_usage(
        provider,
        &mut session.messages,
        session.before_compact.as_deref(),
    )
    .await;
    usage.input_tokens = usage
        .input_tokens
        .saturating_add(outcome.usage_report.usage.input_tokens);
    usage.output_tokens = usage
        .output_tokens
        .saturating_add(outcome.usage_report.usage.output_tokens);
    usage.total_tokens = usage
        .total_tokens
        .saturating_add(outcome.usage_report.usage.total_tokens);
    usage.cached_tokens = usage
        .cached_tokens
        .saturating_add(outcome.usage_report.usage.cached_tokens);
    let mut persistence_failed = false;
    match outcome.result {
        Ok(None) => {
            *status = Status::Notice("nothing old enough to compact yet".to_string());
        }
        Ok(Some(report)) => {
            let summary = format!(
                "compacted {} messages → {} ({} tok → {} tok)",
                report.messages_before,
                report.messages_after,
                report.tokens_before,
                report.tokens_after
            );
            // Only a legacy file log needs rewriting. A v2 session already
            // holds every message in its own append-only `raw.jsonl`, and
            // writing the summarised list to `session_path` as well
            // manufactures a second, lossy session that `/resume` then
            // offers beside the intact one. The automatic path has always
            // been guarded this way (the `TurnOutcome::Done(Ok(_))` arm);
            // this one was not.
            if session.persistence.is_some() {
                *status = Status::Notice(summary);
            } else {
                match persist::rewrite(session_path, &session.messages) {
                    Ok(()) => *status = Status::Notice(summary),
                    Err(e) => {
                        persistence_failed = true;
                        *status = Status::Notice(format!(
                            "compacted in memory, but couldn't persist it: {e}"
                        ));
                    }
                }
            }
        }
        Err(e) => {
            *status = Status::Notice(format!("compaction failed: {e}"));
        }
    }
    persistence_failed
}

/// Runs the `/skills` picker: a blocking loop like the others, but with
/// no selection outcome — Enter and Esc both just close it (see
/// `render::render_skills_picker`'s docs on why: polaris has nothing
/// analogous to codex's enable/disable state to act on). Up/Down move
/// the highlight, and the renderer keeps its page visible.
fn run_skills_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    skills: &[polaris_skills::Skill],
) {
    let mut selected = 0usize;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_skills_picker(f, skills, selected))
            .is_err()
        {
            return;
        }
        if skills.is_empty() {
            let _ = reader.read_key();
            return;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % skills.len(),
            Ok(KeyCode::Up) => selected = (selected + skills.len() - 1) % skills.len(),
            Ok(KeyCode::Enter) | Ok(KeyCode::Esc) => return,
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}

/// What running a resolved slash command should do next, distinct from
/// `ExitCode` so the two call sites (Enter-on-a-highlighted-suggestion,
/// and Enter-on-fully-typed-but-unmatched text) can each decide how to
/// react to `Fatal` without duplicating the `fatal_message`/`break 'outer`
/// wiring twice.
enum SlashOutcome {
    Continue,
    Quit,
    Fatal(String),
    #[cfg(target_os = "linux")]
    Shutdown(signal::LinuxProcessShutdown),
}

#[cfg(target_os = "linux")]
fn append_diff_result_after_read(
    text: &str,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
    observe: impl FnOnce() -> Option<signal::LinuxProcessShutdown>,
) -> Result<(), signal::LinuxProcessShutdown> {
    const MAX_DIFF_LINES: usize = 200;
    if let Some(reason) = observe() {
        return Err(reason);
    }
    local_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
        "diff:",
        ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD),
    )));
    let all_lines: Vec<&str> = text.lines().collect();
    let shown = all_lines.len().min(MAX_DIFF_LINES);
    for raw_line in &all_lines[..shown] {
        local_lines.push(ratatui::text::Line::from(render::sanitize(raw_line)));
    }
    if all_lines.len() > MAX_DIFF_LINES {
        local_lines.push(ratatui::text::Line::from(format!(
            "... truncated, showing {shown} of {} lines",
            all_lines.len()
        )));
    }
    Ok(())
}

/// The file `/init` writes.
///
/// It carries the `## Always on` heading because that heading is the only
/// thing `constitution::extract_always_on` looks for (the alternative is
/// the `<!-- polaris:always-on -->` marker pair). Until 2026-09-19 the
/// template had neither, and its own prose said that everything above the
/// first `## ` heading was sent — so a user who followed it to the letter
/// wrote rules that reached the model zero times, and nothing said so.
fn agents_md_template() -> String {
    let limit = polaris_core::constitution::CONSTITUTION_LIMIT;
    format!(
        "# AGENTS.md\n\n\
         Project-specific instructions for polaris. **Only the `## Always on`\n\
         section below is sent to the model**, and only up to {limit} o200k_base\n\
         tokens of it — lines past that are dropped from the end. A\n\
         `~/.polaris/AGENTS.md` with its own always-on section halves this\n\
         project's share. `polaris doctor` prints which lines reach the model\n\
         and which were dropped.\n\n\
         Everything else in this file is for people. Put long-form material\n\
         under its own `## ` heading; the model can `read` it when it needs it.\n\n\
         ## Always on\n\n\
         - Replace this line with a rule that must hold on every single turn.\n"
    )
}

const REVIEW_INSTRUCTION: &str =
    "Review the current uncommitted changes (`git diff`) and point out any issues you find.";

/// Expands `/review`'s carried text (empty if none was typed) into the
/// actual user message sent to the model. The model already has `bash`
/// and `read` to look at the diff itself — this only supplies the
/// instruction, the same way typing it out by hand would.
fn review_prompt(extra: &str) -> String {
    if extra.is_empty() {
        REVIEW_INSTRUCTION.to_string()
    } else {
        format!("{REVIEW_INSTRUCTION} Focus especially on: {extra}")
    }
}

/// Renders the conversation so far as a plain markdown transcript, for
/// `/export`. Deliberately simple — one `## ` heading per message, role
/// as the heading text, content underneath — rather than trying to
/// reproduce the TUI's own tool-call/tool-result formatting.
fn export_markdown(session: &polaris_core::session::Session) -> String {
    export_messages(&session.messages)
}
fn export_messages(messages: &[polaris_provider::Message]) -> String {
    let mut out = String::from("# polaris conversation\n\n");
    for m in messages {
        let heading = match m.role {
            polaris_provider::Role::User => "You",
            polaris_provider::Role::Assistant => "polaris",
            polaris_provider::Role::Tool => "tool result",
        };
        out.push_str(&format!("## {heading}\n\n"));
        if !m.tool_calls.is_empty() {
            for call in &m.tool_calls {
                out.push_str(&format!("_called `{}`_\n\n", call.name));
            }
        }
        if !m.content.is_empty() {
            out.push_str(&m.content);
            out.push_str("\n\n");
        }
    }
    out
}

/// `Action::Copy`'s handler, pulled out of `apply_slash_action` so its
/// success path — a session with a real reply, flowing through to
/// `*status` — is unit-testable with a fake `copy_fn` instead of only
/// ever being exercised through the real `clipboard::copy_to_clipboard`,
/// which does actual OS-level I/O (unsafe to run unconditionally in
/// automated tests — see `clipboard.rs`'s own tests for why that function
/// is injectable in the first place).
fn apply_copy_action(
    session: &polaris_core::session::Session,
    status: &mut Status,
    copy_fn: impl FnOnce(&str) -> Result<(), String>,
) {
    *status = Status::Notice(clipboard::copy_last_reply_with(session, copy_fn));
}

/// Reveal only a file identified by a completed mouse click.
#[cfg(target_os = "macos")]
fn reveal_history_file(
    view: &mut conversation_view::ConversationViewState,
    mouse: ratatui::crossterm::event::MouseEvent,
    cwd: &Path,
    area: ratatui::layout::Rect,
) -> Option<Status> {
    let path = view
        .file_click
        .mouse(mouse, &view.history, cwd, area, view.scroll_offset)?;
    Some(match file_reveal::reveal(&path) {
        Ok(()) => Status::Notice("Requested file reveal in Finder".into()),
        Err(error) => Status::Error(error),
    })
}

/// `Up`'s copy handler for mouse drag-selection, mirroring
/// `apply_copy_action`'s injectable-`copy_fn` pattern (see its own doc
/// comment for why: real clipboard I/O must never run unconditionally in
/// the test suite). Unlike `apply_copy_action`, this takes the text
/// directly rather than looking it up from a `Session` — the caller
/// (Tasks 8/11) already has it from `selection::extract_text`, and is
/// expected to only call this when `text` is non-empty (an empty/zero-
/// width selection copies nothing and shows no notice at all).
#[cfg(any(test, not(unix)))]
fn apply_selection_copy(text: &str, copy_fn: impl FnOnce(&str) -> Result<(), String>) -> String {
    match copy_fn(text) {
        Ok(()) => "copied selection to the clipboard".to_string(),
        Err(e) => format!("can't copy: {e}"),
    }
}

/// Runs one resolved slash command. Never touches `session.messages` or
/// the model — `Clear` is the only variant here that touches persisted
/// state, and it does so by emptying the session file in place, not by
/// sending anything; `Init`/`Logout` touch the filesystem directly (a new
/// `AGENTS.md`, the deleted credentials file) but likewise never go near
/// the model or `session.messages`. `New` and `Resume` are never actually
/// dispatched here — see their match arms below.
/// What `/mcp` prints. Registration and admission only: nothing is
/// connected to and no model is asked, so this is safe to run at any time
/// and says nothing about whether a server is currently answering.
fn mcp_state_lines(cwd: &std::path::Path, external_enabled: bool) -> Vec<String> {
    let ceiling = polaris_core::mcp::operation::ExternalCeiling::host_external(external_enabled);
    let mut lines = vec![format!(
        "External MCP host ceiling: {}",
        if ceiling.permits_external() {
            "allowed"
        } else {
            "denied"
        }
    )];
    let Ok(host) = polaris_core::mcp::local::LocalHost::open(cwd, false) else {
        lines.push("Cannot read MCP configuration.".into());
        return lines;
    };
    // Show the same external ceiling that the active run actually uses.
    let report = match host.state_report(ceiling) {
        Ok(report) => report,
        Err(error) => {
            lines.push(format!("Cannot read MCP state: {error}"));
            return lines;
        }
    };
    if report.is_empty() {
        lines.push("No registered MCP servers.".into());
        return lines;
    }
    lines.push(format!("{} MCP servers", report.len()));
    for server in report {
        let state = match (&server.quarantine, server.enabled_here) {
            // A refused server is refused whether or not this project
            // enabled it, so the reason comes first.
            (Some(reason), _) => format!("unavailable ({reason})"),
            (None, false) => "disabled for this project".to_owned(),
            (None, true) => "enabled".to_owned(),
        };
        lines.push(format!("  {} — {state}", server.server_id));
    }
    lines
}

#[cfg(unix)]
fn rewind_candidate_diagnostic(
    owner: Option<&polaris_core::rewind_workspace::WorkspaceOwner>,
    sessions_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> Vec<ratatui::text::Line<'static>> {
    let Some(owner) = owner else {
        return vec!["Rewind candidates: cannot verify the workspace owner".into()];
    };
    let Some(data_root) = sessions_dir.parent() else {
        return vec!["Rewind candidates: cannot locate storage".into()];
    };
    let project_root = polaris_core::project::resolve_root(cwd);
    let project_id = match persist::project_identity(&project_root) {
        Ok(project_id) => project_id,
        Err(error) => {
            return vec![format!("Rewind candidates: cannot verify the project: {error}").into()];
        }
    };
    let diagnostics =
        polaris_core::rewind_admission::candidate_diagnostics(owner, data_root, &project_id);
    if diagnostics.is_empty() {
        return vec!["Rewind candidates: none refused or indeterminate".into()];
    }
    let mut lines = vec![
        format!(
            "Rewind candidates: {} refused or indeterminate",
            diagnostics.len()
        )
        .into(),
    ];
    for diagnostic in diagnostics.iter().take(128) {
        lines.push(
            format!(
                "  {}: {}",
                render::sanitize(&diagnostic.id),
                render::sanitize(&diagnostic.reason)
            )
            .into(),
        );
    }
    if diagnostics.len() > 128 {
        lines.push(
            format!(
                "  {} more. Inspect storage while processing is stopped",
                diagnostics.len() - 128
            )
            .into(),
        );
    }
    lines
}

fn doctor_lines(
    provider_name: &str,
    model_name: &str,
    cwd: &std::path::Path,
    sandbox: &polaris_sandbox::SandboxPolicy,
    approval: polaris_core::approval::ApprovalPolicy,
    mcp_external: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let saved_chatgpt = polaris_auth::store::default_path()
        .ok()
        .is_some_and(|path| path.is_file());
    let saved_api_key = polaris_auth::api_key::default_path()
        .ok()
        .is_some_and(|path| path.is_file());
    let env_api_key = std::env::var_os("POLARIS_API_KEY").is_some();
    let project_root = polaris_core::project::resolve_root(cwd);
    let storage_root = std::env::var_os("POLARIS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".polaris"))
        });
    let state = match storage_root {
        Some(root) if root.is_absolute() => {
            let path = root
                .join("state")
                .join(polaris_core::project::project_id(&project_root));
            format!(
                "{} ({})",
                path.display(),
                if path.is_dir() {
                    "present"
                } else {
                    "not created"
                }
            )
        }
        _ => "unavailable (cannot locate storage)".to_owned(),
    };
    let rules = polaris_core::constitution::reach(&project_root);
    let delivered = rules.iter().map(|rule| rule.delivered).sum::<usize>();
    [
        "Diagnostics (read-only)".to_owned(),
        format!("Configuration: provider={provider_name}, model={model_name}"),
        format!(
            "Configuration: ChatGPT credential file={}, API key file={}, environment API key={}",
            saved_chatgpt, saved_api_key, env_api_key
        ),
        format!(
            "Conditions: sandbox={}, approval={approval:?}",
            sandbox.describe()
        ),
        format!("Conditions: external MCP={mcp_external}; inspect individual servers with /mcp"),
        format!("Conditions: state={state}; delivered always-on instruction lines={delivered}"),
        "Observation: this diagnostic does not contact authentication, model, or MCP endpoints. Connection success and actual resource availability are unverified."
            .to_owned(),
    ]
    .into_iter()
    .map(|line| ratatui::text::Line::from(render::sanitize(&line).escape_debug().to_string()))
    .collect()
}

fn cat_diagnostic_lines(
    owner: Option<&polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>,
) -> Vec<ratatui::text::Line<'static>> {
    match owner {
        None => vec![
            status_row("CAT configuration", "no shadow owner"),
            status_row("CAT installation", "not reconciled"),
            status_row("CAT verification", "unverified"),
            status_row("CAT observation", "not retrieved"),
        ],
        Some(owner) => match owner.translation_observation() {
            Ok((requested, installed, verified, attempted)) => vec![
                status_row("CAT configuration", &requested.to_string()),
                status_row(
                    "CAT installation",
                    match installed {
                        Some(true) => "Installation check",
                        Some(false) => "not installed",
                        None => "not reconciled",
                    },
                ),
                status_row("CAT verification", &verified.to_string()),
                status_row(
                    "CAT observation",
                    &format!(
                        "{attempted} attempts (saved observations; distinct from execution success)"
                    ),
                ),
            ],
            Err(_) => vec![status_row(
                "CAT",
                "Cannot read state. Availability is unverified.",
            )],
        },
    }
}

fn account_list_lines() -> Result<Vec<ratatui::text::Line<'static>>, String> {
    use polaris_auth::accounts::store::{AccountState, AccountStore};
    let snapshot = match AccountStore::read_only_snapshot() {
        Ok(snapshot) => snapshot,
        Err(polaris_auth::AuthError::ReadOnly) => {
            let saved = polaris_auth::store::default_path()
                .map_err(|_| "Authentication storage is unverified")?
                .is_file();
            return Ok(vec![status_row(
                "Stored authentication",
                if saved {
                    "present (no registrations; connection unverified)"
                } else {
                    "absent (no registrations)"
                },
            )]);
        }
        Err(_) => {
            return Err(
                "Cannot read registrations. Inspect state with the CLI if recovery is required"
                    .into(),
            );
        }
    };
    let mut lines = vec![status_row(
        "Registrations",
        &snapshot.accounts.len().to_string(),
    )];
    for account in snapshot.accounts {
        let state = match account.state {
            AccountState::Registering => "registering",
            AccountState::Ready => "credentials present",
            AccountState::RefreshPending => "updating",
            AccountState::ReauthRequired => "login required",
            AccountState::Disconnected => "logged out",
        };
        lines.push(status_row(
            "Registration",
            &format!(
                "{} / {} / {state}",
                account.registration_id,
                account.label.as_deref().unwrap_or("no display name")
            ),
        ));
    }
    Ok(lines)
}

fn memory_read_lines(
    cwd: &std::path::Path,
    state_dir: &std::path::Path,
    sessions_dir: &std::path::Path,
    request: &str,
) -> Result<Vec<ratatui::text::Line<'static>>, String> {
    use polaris_memory::{MemoryStore, SearchMode, SearchRequest};

    const USAGE: &str = "/memory search <query> | /memory get <session> <id> | /memory source <session> <id-or-conversation-URI>";
    if request.is_empty() {
        return Ok(vec![ratatui::text::Line::from(USAGE)]);
    }
    if request.len() > 1024 || request.chars().any(char::is_control) {
        return Err("Specify a memory query as one line within 1024 bytes".into());
    }
    let mut parts = request.split_whitespace();
    let operation = parts.next().unwrap_or("");
    if !matches!(operation, "search" | "get" | "source") {
        return Err(format!("Memory operations available in this TUI: {USAGE}"));
    }
    let project_id = persist::project_identity(cwd)
        .map_err(|error| format!("Cannot identify the project: {error}"))?;
    let store = MemoryStore::open_read_only(state_dir.join("memory.sqlite3"))
        .map_err(|error| format!("Cannot open saved memory: {error}"))?;
    let mut lines = vec![ratatui::text::Line::from(
        "Historical records, not current instructions or approvals.",
    )];
    match operation {
        "search" => {
            let query = request.strip_prefix("search").unwrap_or("").trim();
            if query.is_empty() {
                return Err(format!("A search query is required: {USAGE}"));
            }
            let hits = store
                .search(&SearchRequest {
                    project_id: &project_id,
                    session_id: None,
                    mode: SearchMode::Lexical(query),
                    limit: 5,
                    excerpt_bytes: 512,
                })
                .map_err(|error| format!("Cannot search memory: {error}"))?;
            if hits.is_empty() {
                lines.push(ratatui::text::Line::from("No matching records"));
            }
            for hit in hits {
                lines.push(ratatui::text::Line::from(render::sanitize(&format!(
                    "{} / {} / {} / {}",
                    hit.session_id, hit.id, hit.timestamp, hit.source
                ))));
                lines.push(ratatui::text::Line::from(render::sanitize(
                    &hit.excerpt.replace(['\n', '\r'], " "),
                )));
            }
        }
        "get" => {
            let session = parts.next().ok_or_else(|| USAGE.to_string())?;
            let id = parts.next().ok_or_else(|| USAGE.to_string())?;
            let start = parts
                .next()
                .map(str::parse::<usize>)
                .transpose()
                .map_err(|_| "start is a UTF-8 byte offset")?
                .unwrap_or(0);
            let budget = parts
                .next()
                .map(str::parse::<usize>)
                .transpose()
                .map_err(|_| "max-bytes must be an integer")?
                .unwrap_or(4096);
            if parts.next().is_some() || budget > 4096 {
                return Err("/memory get <session> <id> [start] [max-bytes:0..4096]".into());
            }
            let range = store
                .get_range(&project_id, session, id, start, budget)
                .map_err(|error| format!("Cannot retrieve memory: {error}"))?
                .ok_or("No matching records")?;
            lines.push(ratatui::text::Line::from(render::sanitize(&format!(
                "{} / {} / {} / {} ({} / {} bytes)",
                range.record.session_id,
                range.record.id,
                range.record.timestamp,
                range.record.source,
                range.end,
                range.total_bytes
            ))));
            for text in range.record.text.lines() {
                lines.push(ratatui::text::Line::from(render::sanitize(text)));
            }
            if range.end < range.total_bytes {
                lines.push(ratatui::text::Line::from(
                    "Continue with /memory get <session> <id> <displayed-byte-offset>.",
                ));
            }
        }
        "source" => {
            let session = parts.next().ok_or_else(|| USAGE.to_string())?;
            let id = parts.next().ok_or_else(|| USAGE.to_string())?;
            if parts.next().is_some()
                || session.is_empty()
                || !session
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(USAGE.to_string());
            }
            let uri = if id.starts_with("conversation://") {
                id.to_owned()
            } else if id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                format!("conversation://{id}")
            } else {
                return Err("Specify a source ID or conversation:// URI".into());
            };
            let data_root = sessions_dir
                .parent()
                .ok_or("Cannot locate conversation storage")?;
            let persisted = polaris_core::session_store::PersistedSession::open(
                data_root,
                &state_dir.join("memory.sqlite3"),
                &project_id,
                session,
            )
            .map_err(|error| format!("Cannot open the saved conversation: {error}"))?;
            let source = polaris_core::conversation_memory::read_source(
                persisted.store.as_ref(),
                &state_dir.join("memory.sqlite3"),
                &uri,
            )
            .map_err(|error| format!("Cannot verify the source: {error}"))?;
            for text in source.lines() {
                lines.push(ratatui::text::Line::from(render::sanitize(text)));
            }
        }
        _ => unreachable!("operation was validated"),
    }
    Ok(lines)
}

fn decided_read_lines(cwd: &std::path::Path) -> Result<Vec<ratatui::text::Line<'static>>, String> {
    use std::io::Read;

    let root = polaris_core::project::resolve_root(cwd);
    let ledger = root.join(polaris_core::decisions::PATH);
    let mut data = Vec::new();
    match std::fs::File::open(&ledger) {
        Ok(file) => file
            .take(1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map_err(|error| format!("Cannot read decisions: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(format!("Cannot read decisions: {error}")),
    };
    if data.len() > 1024 * 1024 {
        return Err("The decision ledger exceeds 1 MiB and cannot be displayed in TUI".into());
    }
    let text = String::from_utf8(data).map_err(|_| "The decision ledger is not UTF-8")?;
    let entries = polaris_core::decisions::parse(&text);
    let mut lines = vec![ratatui::text::Line::from(format!(
        "{} recorded decisions. Historical records, not current approvals.",
        entries.len()
    ))];
    if entries.is_empty() {
        lines.push(ratatui::text::Line::from("No records"));
    }
    let omitted = entries.len().saturating_sub(100);
    if omitted > 0 {
        lines.push(ratatui::text::Line::from(format!(
            "Omitted {omitted} older records"
        )));
    }
    for entry in entries.into_iter().skip(omitted) {
        lines.push(ratatui::text::Line::from(render::sanitize(&format!(
            "{}  {}",
            entry.at, entry.what
        ))));
    }
    Ok(lines)
}

fn compare_read_lines(
    cwd: &std::path::Path,
    request: &str,
) -> Result<Vec<ratatui::text::Line<'static>>, String> {
    use polaris_core::similarity::{self, Suggestion};
    const USAGE: &str = "/compare <folder1> | <folder2> [| <folder3> ...]";
    if request.is_empty() {
        return Ok(vec![ratatui::text::Line::from(USAGE)]);
    }
    if request.len() > 2048 || request.chars().any(char::is_control) {
        return Err("Specify folders to compare as one line within 2048 bytes".into());
    }
    let folders = request.split('|').map(str::trim).collect::<Vec<_>>();
    if !(2..=8).contains(&folders.len()) || folders.iter().any(|path| path.is_empty()) {
        return Err(format!("{USAGE} (2–8 folders)"));
    }
    let mut samples = Vec::with_capacity(folders.len());
    let mut lines = Vec::new();
    for raw in folders {
        let path = std::path::Path::new(raw);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let id = full.display().to_string();
        let sampled = similarity::sample::folder(&id, &full)
            .map_err(|error| format!("Cannot read {}: {error}", render::sanitize(&id)))?;
        if sampled.truncated {
            lines.push(ratatui::text::Line::from(render::sanitize(&format!(
                "{id}: partial comparison. A low similarity result is not conclusive"
            ))));
        }
        samples.push(sampled.folder);
    }
    let graph = similarity::graph(&samples);
    if graph.omitted > 0 {
        lines.push(ratatui::text::Line::from(format!(
            "{} items were not compared because of the limit",
            graph.omitted
        )));
    }
    if graph.edges.is_empty() {
        lines.push(ratatui::text::Line::from("No similar folders found"));
    }
    for edge in graph.edges {
        lines.push(ratatui::text::Line::from(render::sanitize(&format!(
            "{} / {}: similarity {}, {} shared items",
            edge.left,
            edge.right,
            edge.similarity,
            edge.shared.len()
        ))));
    }
    for proposal in graph.proposals {
        let why = match proposal.suggestion {
            Suggestion::SameWorkInTwoPlaces => "The same work may exist in two locations",
            Suggestion::SharedPart => "A shared component may be extractable",
        };
        lines.push(ratatui::text::Line::from(render::sanitize(&format!(
            "Suggestion: {} / {}: {why}. {}",
            proposal.left, proposal.right, proposal.because
        ))));
    }
    lines.push(ratatui::text::Line::from(
        "Suggestions are investigation leads. Nothing is moved or deleted.",
    ));
    Ok(lines)
}

fn clear_mcp_prompt_on_session_change(
    previous: &std::path::Path,
    current: &std::path::Path,
    pending: &mut Option<polaris_core::mcp::runtime::McpPromptChoice>,
    requires_reselect: &mut bool,
) {
    if previous != current {
        *pending = None;
        *requires_reselect = false;
    }
}

fn mcp_prompt_choice(request: &str) -> Result<polaris_core::mcp::runtime::McpPromptChoice, String> {
    if request.len() > 2048 || request.chars().any(char::is_control) {
        return Err("Specify MCP prompt selection as one line within 2048 bytes".into());
    }
    let mut parts = request.split_whitespace();
    if parts.next() != Some("prompt") {
        return Err("/mcp prompt <server>/<prompt> [key=value ...]".into());
    }
    let spec = parts
        .next()
        .ok_or("/mcp prompt <server>/<prompt> [key=value ...]")?;
    let (server, prompt) = spec
        .split_once('/')
        .ok_or("Specify the MCP prompt as server/prompt")?;
    if server.is_empty() || prompt.is_empty() {
        return Err("Specify both the MCP server and prompt".into());
    }
    let mut arguments = serde_json::Map::new();
    for argument in parts {
        if arguments.len() >= 16 {
            return Err("At most 16 MCP prompt arguments are allowed".into());
        }
        let (key, value) = argument
            .split_once('=')
            .ok_or("Specify MCP prompt arguments as key=value")?;
        if key.is_empty() || arguments.contains_key(key) {
            return Err("An MCP prompt argument key is empty or duplicated".into());
        }
        arguments.insert(key.into(), serde_json::Value::String(value.into()));
    }
    Ok(polaris_core::mcp::runtime::McpPromptChoice {
        server: server.into(),
        prompt: prompt.into(),
        arguments,
    })
}

#[cfg(unix)]
fn management_effect_diagnostic(
    project: &Path,
    store: &polaris_core::management_effect::ManagementEffectStoreV1,
    recovery_issue: Option<&str>,
) -> Vec<ratatui::text::Line<'static>> {
    let mut lines = match polaris_core::sync::composite::C10CompositeBlockerV1::inspect_readonly(
        project,
        store.state_dir(),
    ) {
        Ok(Some(effect)) => vec![
            status_row("Management operations", "Unresolved"),
            status_row("Operation ID", &effect.request.operation_id.to_string()),
            status_row("Target", &effect.request.inventory.to_string()),
            status_row("State", &format!("{:?}", effect.state)),
            status_row("Record number", &effect.last_sequence.to_string()),
        ],
        Ok(None) => vec![status_row("Management operations", "none unresolved")],
        Err(error) => vec![status_row(
            "Management operations",
            &format!("Cannot verify state ({error})"),
        )],
    };
    if let Some(reason) = recovery_issue {
        lines.push(status_row("Recovery reason", reason));
    }
    lines
}

#[cfg(unix)]
fn management_clearance_error(
    project: &Path,
    store: &polaris_core::management_effect::ManagementEffectStoreV1,
    action: &slash::Action,
) -> Option<String> {
    if !action.needs_management_clearance() {
        return None;
    }
    match polaris_core::sync::composite::C10CompositeBlockerV1::inspect_readonly(
        project,
        store.state_dir(),
    ) {
        Ok(Some(effect)) => Some(format!(
            "Unresolved management operation {} ({}). Inspect state first",
            effect.request.operation_id, effect.request.inventory
        )),
        Ok(None) => None,
        Err(error) => Some(format!("Cannot verify management operation state: {error}")),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_slash_action(
    action: slash::Action,
    session: &mut polaris_core::session::Session,
    session_path: &std::path::Path,
    status: &mut Status,
    _provider_name: &str,
    _model_name: &str,
    _usage_report: polaris_provider::UsageReport,
    // No longer read here — `/skills` is intercepted in `run()` before
    // dispatch (see `run_skills_picker`) now that it's a full-screen
    // picker instead of a one-line `Status::Notice`. Kept as a parameter
    // rather than removed, since removing it would mean touching every
    // `apply_slash_action`/`call` call site (production and test) for a
    // signature change with no behavior difference.
    _skills: &[polaris_skills::Skill],
    cwd: &std::path::Path,
    state_dir: &std::path::Path,
    sessions_dir: &std::path::Path,
    mcp_external: bool,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) -> SlashOutcome {
    if let Err(error) = require_action_owner(&action) {
        *status = Status::Error(error.into());
        return SlashOutcome::Continue;
    }
    match action {
        slash::Action::SessionControl(_)
        | slash::Action::Environment(_)
        | slash::Action::WorkspaceControl(_)
        | slash::Action::RuntimeControl(_) => {
            *status = Status::Error(
                "This operation requires an active terminal and storage owner".into(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Quit => SlashOutcome::Quit,
        slash::Action::Clear => {
            local_lines.clear();
            let cleared = if session.persistence.is_some() {
                session.clear_history()
            } else {
                session.messages.clear();
                persist::clear_session(session_path)
            };
            match cleared {
                Ok(()) => {
                    *status = Status::Idle;
                    SlashOutcome::Continue
                }
                Err(e) => SlashOutcome::Fatal(format!("Can't clear the persisted session: {e}")),
            }
        }
        slash::Action::Status => {
            *status = Status::Error("Cannot verify the /status display path".into());
            SlashOutcome::Continue
        }
        slash::Action::Account(_) => {
            *status = Status::Error(
                "Authentication operations require an active terminal and storage owner".into(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Memory(requested) => {
            match memory_read_lines(cwd, state_dir, sessions_dir, &requested) {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("Saved memory reference result displayed".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        slash::Action::Decided(requested) => {
            if !requested.is_empty() && requested != "list" {
                *status = Status::Error("/decided currently supports only list".into());
                return SlashOutcome::Continue;
            }
            match decided_read_lines(cwd) {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("Decisions displayed".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        slash::Action::Compare(requested) => {
            match compare_read_lines(cwd, &requested) {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("Folder comparison displayed".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        #[cfg(target_os = "linux")]
        slash::Action::Apply(_) => {
            *status = Status::Error(
                "D27 operations require a real terminal and source-bound owner".into(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Sync(_) => {
            *status = Status::Error(
                "Shared apply requires a supported OS confirmation screen and sharing owner".into(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Phase(requested) => {
            *status = if requested.is_empty() {
                session.workflow.as_ref().map_or_else(
                    || Status::Notice("Workflow is disabled for this session".into()),
                    |workflow| {
                        Status::Notice(format!("Workflow phase: {}", workflow.state().phase))
                    },
                )
            } else {
                Status::Error(
                    "Phase changes require a management operation confirmation screen".into(),
                )
            };
            SlashOutcome::Continue
        }
        slash::Action::Workflow(requested) => {
            *status = if requested.is_empty() {
                session.workflow.as_ref().map_or_else(
                    || Status::Notice("Workflow is disabled for this session".into()),
                    |workflow| {
                        let mode = if workflow.config.enabled {
                            match workflow.config.mode {
                                polaris_core::workflow::WorkflowMode::Manual => "manual",
                                polaris_core::workflow::WorkflowMode::Adaptive => "adaptive",
                            }
                        } else {
                            "off"
                        };
                        Status::Notice(format!("workflow: {mode}"))
                    },
                )
            } else {
                Status::Error(
                    "Workflow changes require a management operation confirmation screen".into(),
                )
            };
            SlashOutcome::Continue
        }
        slash::Action::Init => {
            // At the project root, because that is the only `AGENTS.md`
            // that is ever read. Writing it beside a subdirectory the run
            // happened to start in produces a file nothing loads.
            let path = polaris_core::project::resolve_root(cwd).join("AGENTS.md");
            if path.exists() {
                *status = Status::Notice(format!("{} already exists", path.display()));
            } else {
                match std::fs::write(&path, agents_md_template()) {
                    Ok(()) => *status = Status::Notice(format!("created {}", path.display())),
                    Err(e) => {
                        return SlashOutcome::Fatal(format!(
                            "Can't create {}: {e}",
                            path.display()
                        ));
                    }
                }
            }
            SlashOutcome::Continue
        }
        slash::Action::Pwd => {
            *status = Status::Notice(
                polaris_core::project::resolve_root(cwd)
                    .display()
                    .to_string(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Mcp(requested) => {
            if !requested.is_empty() && requested != "list" {
                *status = Status::Error(
                    "/mcp prompt selection is handled by the input admission screen".into(),
                );
                return SlashOutcome::Continue;
            }
            for line in mcp_state_lines(cwd, mcp_external) {
                local_lines.push(ratatui::text::Line::from(line));
            }
            *status = Status::Idle;
            SlashOutcome::Continue
        }
        slash::Action::Copy => {
            apply_copy_action(session, status, clipboard::copy_to_clipboard);
            SlashOutcome::Continue
        }
        slash::Action::Export(destination) => {
            let path = if destination.is_empty() {
                cwd.join(format!("polaris-export-{}.md", now_millis()))
            } else {
                cwd.join(&destination)
            };
            match std::fs::write(&path, export_markdown(session)) {
                Ok(()) => *status = Status::Notice(format!("exported to {}", path.display())),
                Err(e) => {
                    return SlashOutcome::Fatal(format!("Can't write {}: {e}", path.display()));
                }
            }
            SlashOutcome::Continue
        }
        slash::Action::Diff => {
            #[cfg(not(unix))]
            {
                *status = Status::Error("/diff is not yet available on Windows".into());
                SlashOutcome::Continue
            }
            #[cfg(unix)]
            {
                #[cfg(not(target_os = "linux"))]
                const MAX_DIFF_LINES: usize = 200;
                let text = (|| -> Result<String, String> {
                    let project_root = polaris_core::project::resolve_root(cwd);
                    let root = std::fs::File::open(project_root)
                        .map_err(|_| "Cannot retrieve Git information".to_string())?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    let read = |args: &[&str]| {
                        polaris_core::project_git::read_only_git(&root, args, deadline)
                            .map_err(|error| error.to_string())
                    };
                    let mut text = String::from_utf8_lossy(&read(&[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                    ])?)
                    .into_owned();
                    let staged = read(&["diff", "--cached", "--no-ext-diff", "--no-textconv"])?;
                    if !staged.is_empty() {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&String::from_utf8_lossy(&staged));
                    }
                    let untracked = read(&["ls-files", "--others", "--exclude-standard", "-z"])?;
                    for path in untracked
                        .split(|byte| *byte == 0)
                        .filter(|path| !path.is_empty())
                    {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str("?? ");
                        text.push_str(&String::from_utf8_lossy(path).escape_debug().to_string());
                        text.push('\n');
                    }
                    Ok(if text.is_empty() {
                        "(no changes)".to_string()
                    } else {
                        text
                    })
                })()
                .unwrap_or_else(|error| error);
                #[cfg(target_os = "linux")]
                if let Err(reason) = append_diff_result_after_read(
                    &text,
                    local_lines,
                    signal::pending_linux_process_shutdown,
                ) {
                    return SlashOutcome::Shutdown(reason);
                }
                #[cfg(not(target_os = "linux"))]
                {
                    local_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                        "diff:",
                        ratatui::style::Style::default()
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )));
                    let all_lines: Vec<&str> = text.lines().collect();
                    let shown = all_lines.len().min(MAX_DIFF_LINES);
                    for raw_line in &all_lines[..shown] {
                        local_lines.push(ratatui::text::Line::from(render::sanitize(raw_line)));
                    }
                    if all_lines.len() > MAX_DIFF_LINES {
                        local_lines.push(ratatui::text::Line::from(format!(
                            "... truncated, showing {shown} of {} lines",
                            all_lines.len()
                        )));
                    }
                }
                *status = Status::Idle;
                SlashOutcome::Continue
            }
        }
        slash::Action::Review(_)
        | slash::Action::Doctor
        | slash::Action::New
        | slash::Action::Resume
        | slash::Action::Permissions
        | slash::Action::Fork
        | slash::Action::Model
        | slash::Action::Skills
        | slash::Action::Compact => {
            // Never reached: `run()` recognizes all of these before
            // dispatch — `Review` expands into a normal model turn;
            // `New`/`Resume`/`Fork` are handled by
            // `handle_new_session`/`handle_resume`/`handle_fork`;
            // `Permissions`/`Model`/`Skills` are handled by
            // `handle_permissions`/`handle_model`/`run_skills_picker`.
            // Every one of them needs state (`session_path`,
            // `approval_policy`, `model_name`, the terminal, ...) this
            // function doesn't have. This arm exists only so the match
            // stays exhaustive — if it's ever hit, fail soft rather than
            // panic over one command.
            *status = Status::Notice(
                "internal: this command should have been handled before dispatch. \
                                 Please report this as a polaris bug."
                    .to_string(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Logout => {
            *status = Status::Error(
                "internal: /logout should have been handled by the account dispatcher".into(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Help => {
            local_lines.push(ratatui::text::Line::from("commands:"));
            for command in slash::COMMANDS {
                local_lines.push(ratatui::text::Line::from(format!(
                    "/{} — {}",
                    command.name, command.description
                )));
            }
            *status = Status::Notice("commands listed above (scroll to see all)".to_string());
            SlashOutcome::Continue
        }
        slash::Action::Unknown(name) => {
            *status = Status::Notice(format!("unknown command: /{name} (try /help)"));
            SlashOutcome::Continue
        }
    }
}

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/tool_memory_settings.rs"]
mod tool_memory_settings_tests;
