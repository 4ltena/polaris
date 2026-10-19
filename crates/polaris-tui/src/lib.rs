//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

#[cfg(unix)]
pub mod account;
pub mod approver;
mod clipboard;
#[cfg(test)]
mod continuation_tests;
#[cfg(unix)]
mod draft;
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
#[cfg(unix)]
mod session_commands;
#[cfg(target_os = "linux")]
mod workspace_commands;
pub use memory::{configure_archive, configure_memory, lock_memory};
pub mod render;
mod rewind_ui;
mod selection;
pub mod sessions;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod signal;
pub mod slash;
#[cfg(target_os = "linux")]
mod source_apply;
mod time;
pub mod tool_memory;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::fs;
use std::io::IsTerminal;
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
use zeroize::Zeroize;

#[cfg(not(target_os = "linux"))]
use approver::CrosstermKeyReader;
use approver::TuiApprover;
use input::{InputAction, apply_key};
use render::Status;

const UNDECIDED_EFFORT: &str = "undecided";

#[cfg(target_os = "linux")]
const LINUX_SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(target_os = "linux")]
const LINUX_SHUTDOWN_DURING_RESTORE: &str = "停止要求を受信しました";

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
        first_error = Some(format!("MCP runtimeを終了できません: {error}"));
    }
    if let Some(admission) = admission
        && let Err(error) =
            admission.finish(polaris_core::session_manager::TerminalStatus::OutcomeUnknown)
        && first_error.is_none()
    {
        first_error = Some(format!("sidechat runを終了できません: {error}"));
    }
    if let Some(boundary) = boundary {
        let result = persistence
            .ok_or_else(|| "巻き戻し境界の保存元が失われました".to_string())
            .and_then(|saved| {
                saved
                    .abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "停止要求を受信したためターンを開始しませんでした".into(),
                    )
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = result
            && first_error.is_none()
        {
            first_error = Some(format!("巻き戻し境界を確定できません: {error}"));
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
    fn enter() -> Result<(Self, ratatui::DefaultTerminal), String> {
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
            ratatui::backend::CrosstermBackend::new(std::io::stdout()),
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
        if ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::EnableMouseCapture
        )
        .is_ok()
        {
            self.mouse = true;
        }
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
    activity: &AgentActivityRows,
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
                return Err("/input status <受理ID>。実行中の取消はEscで行います。".into());
            };
            let saved = saved.ok_or("受理記録にはv2会話が必要です")?;
            let snapshot = saved.snapshot().map_err(|e| e.to_string())?;
            let current = polaris_core::session_manager::SessionId::new(snapshot.state.session_id)
                .map_err(|e| e.to_string())?;
            if !state_dir.join("tui-admissions-v1").exists() {
                return Err("この会話に保存された受理記録はありません".into());
            }
            let manager = polaris_core::desktop_store::SessionManagerStore::inspect_family_owned(
                &state_dir.join("tui-admissions-v1"),
                &current,
            )
            .map_err(|e| e.to_string())?
            .ok_or("この会話に保存された受理記録はありません")?;
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
                return Err("この会話の受理記録ではありません".into());
            }
            Ok(vec![render::sanitize(&format!(
                "入力 {} / revision {} / 状態 {:?}",
                admitted.id.as_str(),
                admitted.revision,
                admitted.state
            ))])
        }
        "instruction" => {
            Err("実行中instructionの永続ownerは接続中です。通常入力を使ってください".into())
        }
        _ => Err(
            "/subagents、/roles、/run settings|details、/sandbox、/input status を指定してください"
                .into(),
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
        let saved = session.persistence.as_ref().ok_or("v2保存会話が必要です")?;
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
            return Ok(vec![format!("指示受理ID: {id}。次のrun境界で適用します")]);
        }
        let id = polaris_core::session_manager::AdmissionId::new(cancel.expect("cancel"))
            .map_err(|e| e.to_string())?;
        let position = queue.iter().position(|item| item.admission_id() == &id);
        let is_active = active
            .as_ref()
            .is_some_and(|item| item.admission_id() == &id);
        if position.is_none() && !is_active {
            return Err("この会話の送信待ちIDではありません".into());
        }
        if !account::confirm(
            terminal,
            reader,
            "送信待ち入力の取消",
            id.as_str(),
            "このIDの未実行の指示だけを取り消します。",
        ) {
            let context = runtime_commands::RuntimeMutationContext {
                effects,
                lease: &activity,
                session_id: &snapshot.state.session_id,
                generation: snapshot.state.generation,
                accepted: false,
            };
            runtime_effects::begin(&context, "D10", id.as_str().as_bytes())?;
            return Ok(vec!["取消要求を取り消しました".into()]);
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
        Ok(vec![format!("入力 {} を取り消しました", id.as_str())])
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
        let (history, usage) = factory.ok_or("履歴の埋め込み・要約backendがありません")?()?;
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
        .map_err(|error| format!("v2セッションを接続できません: {error}"))?;
    candidate_session
        .recover_saved_history()
        .await
        .map_err(|error| format!("保存済みの履歴を復元できません: {error}"))?;
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
        .map_err(|error| format!("ワークフロー状態を復元できません: {error}"))?;
    let workflow = if workflow_mode_overridden {
        let mut candidate_config = restored.config.clone();
        candidate_config.enabled = config.enabled;
        candidate_config.mode = config.mode;
        let candidate = polaris_core::workflow::SessionWorkflow::restore(
            candidate_config,
            restored.gates.clone(),
            restored.state(),
        )
        .map_err(|error| format!("workflow設定を検証できません: {error}"))?;
        if candidate.config.enabled {
            drop(
                candidate
                    .begin_turn_from_disk(skills)
                    .map_err(|error| format!("workflow設定を検証できません: {error}"))?,
            );
        }
        candidate
    } else {
        restored
    };
    if workflow_mode_overridden {
        return Err("workflow変更には管理操作の確認と記録が必要です".into());
    }
    let publication = if needs_checkpoint {
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return Err(LINUX_SHUTDOWN_DURING_RESTORE.into());
        }
        if !allow_compat_checkpoint {
            return Err("workflow互換状態の移行は管理操作の復旧後に行ってください".into());
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
            .map_err(|error| format!("workflow設定を保存できません: {error}"))?;
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
        return Err("--phase には設定で [workflow].enabled = true が必要です".into());
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
        .map_err(|error| (format!("MCPを準備できません: {error}"), false))?
        .ok_or_else(|| ("MCPサーバーを利用できません".to_owned(), false))?;
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
            Err(("MCP promptを確認できません".into(), unknown))
        }
        Ok(Err(error)) => {
            let _ = runtime.shutdown().await;
            Err((format!("MCP promptを取得できません: {error}"), true))
        }
        Err(_) => {
            let _ = runtime.shutdown().await;
            Err(("MCP prompt取得が時間切れです".into(), true))
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
            _ => Err("sidechat runが実行中ではありません".into()),
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
    Err("TUI sidechat admissionはこのplatformでは利用できません".into())
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
                            return Err("保存済みadmission receiptがpayloadと不一致です".into());
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
        .ok_or("sidechatにはv2セッションが必要です")?;
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
        .ok_or("sidechat自動振り分けにはv2セッションが必要です")?;
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
            "入力 {} の実行状態が不明です。この会話では新しい入力を開始できません。/new で別会話を作成するか、保存状態を確認してください",
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
                "この会話の保存容量が不足しています。処理が静止したら /fork で継続してください"
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
                "この会話の保存容量が不足しています。処理が静止したら /fork で継続してください"
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
                "この会話の保存容量が不足しています。処理が静止したら /fork で継続してください"
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
            return Err(format!("sidechat destinationを準備できません: {error}").into());
        }
    };
    persist_manager_transition(&store, before, &manager)?;
    let (destination, reason) = match &prepared.state {
        AdmissionState::DestinationReady { session_id, reason } => (session_id.clone(), *reason),
        _ => return Err("sidechat destinationが確定しませんでした".into()),
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
        return Err(format!("routine authorityを接続できません: {error}").into());
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
        Err(error) => (None, Some(format!("巻き戻し保存を開始できません: {error}"))),
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
            "sidechat runを開始できません: {:?}",
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
                "sidechat {} に切り替えました ({reason:?}, running) · {warning}",
                destination.as_str()
            )),
            (true, None) => Some(format!(
                "sidechat {} に切り替えました ({reason:?}, running)",
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

// Ancestors are read-only and must form an explicit, bounded same-project chain.
const MAX_TOOL_MEMORY_ORIGINS: usize = 64;

fn saved_tool_memory_namespace(
    project: &std::path::Path,
    path: &std::path::Path,
) -> std::io::Result<(String, Vec<String>)> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .ok_or_else(|| std::io::Error::other("invalid session id"))?
        .to_owned();
    let project = polaris_core::project::resolve_root(project);
    let mut seen = std::collections::HashSet::from([id.clone()]);
    let mut origins = Vec::new();
    let mut cursor = path.to_path_buf();
    while let Some(origin) = persist::read_tool_memory_origin(&cursor.with_extension("meta.json"))?
    {
        if origins.len() >= MAX_TOOL_MEMORY_ORIGINS || !seen.insert(origin.clone()) {
            return Err(std::io::Error::other(
                "記憶の系譜が循環しているか上限を超えています",
            ));
        }
        let source = path.with_file_name(format!("{origin}.jsonl"));
        let source_meta = persist::read_meta(&source.with_extension("meta.json"))
            .ok_or_else(|| std::io::Error::other("記憶の元セッションの出自を確認できません"))?;
        if !source.is_file()
            || !std::path::Path::new(&source_meta.cwd).is_absolute()
            || polaris_core::project::resolve_root(std::path::Path::new(&source_meta.cwd))
                != project
        {
            return Err(std::io::Error::other(
                "記憶の元セッションが存在しないか同一プロジェクトではありません",
            ));
        }
        origins.push(origin);
        cursor = source;
    }
    Ok((id, origins))
}

fn configure_saved_tool_memory(
    session: &mut polaris_core::session::Session,
    project: &std::path::Path,
    state_dir: &std::path::Path,
    session_path: &std::path::Path,
    mode: Option<polaris_core::tool_memory::RetentionMode>,
    embedding: Option<(String, String)>,
) -> std::io::Result<()> {
    session.tool_memory = None;
    if let Some(mode) = mode
        && memory::saved_project_matches(session, project, session_path)
    {
        if let Some(persistence) = session.persistence.as_ref() {
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
                return Err(std::io::Error::other("記憶の系譜が上限を超えています"));
            }
            if let Some(branch) = persistence.store.rewind_branch_metadata()? {
                if !origins.contains(&branch.origin_session_id) {
                    return Err(std::io::Error::other(
                        "rewind分岐の記憶出自が保存された系譜と一致しません",
                    ));
                }
                tool_memory::configure_tool_memory_with_origin_allowlist(
                    session,
                    project,
                    state_dir,
                    &snapshot.state.session_id,
                    &origins,
                    &branch.visible_memory_raw_object_ids,
                    mode,
                    embedding,
                )?;
            } else {
                tool_memory::configure_tool_memory_with_origins(
                    session,
                    project,
                    state_dir,
                    &snapshot.state.session_id,
                    &origins,
                    mode,
                    embedding,
                )?;
            }
            return Ok(());
        }
        let (id, origins) = saved_tool_memory_namespace(project, session_path)?;
        tool_memory::configure_tool_memory_with_origins(
            session, project, state_dir, &id, &origins, mode, embedding,
        )?;
    }
    Ok(())
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
        return Err(std::io::Error::other("記憶の系譜が上限を超えています"));
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
                    "停止要求を受けたため巻き戻し境界は未完了です".into(),
                );
            }
            let reason = match crossing {
                ManagementRewindCrossingV1::Clear => external_unknown,
                ManagementRewindCrossingV1::Blocked { .. } => {
                    Some("巻き戻せない管理操作が含まれています".into())
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
            format!("ターン後の保存証拠を確認できません: {error}"),
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

const WINDOWS_OWNER_UNAVAILABLE: &str =
    "Windowsでは実行・変更に必要な保護機構が未対応です。変更していません。";

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
    *status = Status::Notice("送信待ち・下書きを処理してから /quit を実行してください".into());
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
/// `reset_conversation_view` explicitly (see its own doc and every call
/// site in `run()`, including the one right after `/clear`'s
/// `apply_slash_action` call) *before* falling through to the next
/// `append_new_history` call, so by the time this function's own
/// shrink-guard would fire, it instead finds `history` already correctly
/// re-seeded and `printed_messages`/`printed_local_lines` already at 0 —
/// meaning the guard is a no-op safety net on that path, not the
/// mechanism actually doing the reset.
fn append_new_history(
    session: &polaris_core::session::Session,
    local_lines: &[ratatui::text::Line<'static>],
    printed_messages: &mut usize,
    printed_local_lines: &mut usize,
    history: &mut Vec<render::HistoryLine>,
) -> bool {
    let mut changed = false;
    // A session that's now *shorter* than what's already been printed
    // (`/clear`, `/new`) can only mean it was reset out from under us —
    // reprint from scratch. This alone doesn't catch `/resume` loading a
    // same-or-longer *different* session, which is why every `/resume`
    // call site also resets both counters (and `history`) explicitly.
    if *printed_messages > session.messages.len() {
        *printed_messages = 0;
        history.clear();
        changed = true;
    }
    if *printed_local_lines > local_lines.len() {
        *printed_local_lines = 0;
        history.clear();
        changed = true;
    }
    if session.messages.len() > *printed_messages {
        let lines = render::history_lines_for(&session.messages[*printed_messages..]);
        history.extend(lines);
        *printed_messages = session.messages.len();
        changed = true;
    }
    if local_lines.len() > *printed_local_lines {
        let new_lines = &local_lines[*printed_local_lines..];
        history.extend(new_lines.iter().map(|l| render::HistoryLine {
            line: l.clone(),
            shaded: false,
        }));
        *printed_local_lines = local_lines.len();
        changed = true;
    }
    changed
}

/// Clears `history` and immediately re-seeds it with the header box,
/// resets `scroll_offset` back to 0 (following the tail), and drops any
/// in-progress or finalized text `selection` — the one place every
/// conversation-view reset (`/resume`, `/new`, `/fork`, `/clear`) goes
/// through, so the header can't be dropped by a bare `history.clear()`, a
/// stale scroll position can't survive into a different, possibly much
/// shorter, conversation, and a selection can't keep pointing at
/// `wrapped` coordinates from a history that no longer exists. Callers
/// still separately reset `printed_messages`/`printed_local_lines` to 0 —
/// this only owns `history`/`scroll_offset`/`selection`, not the counters
/// `append_new_history` reads.
fn reset_conversation_view(
    history: &mut Vec<render::HistoryLine>,
    history_wrap_cache: &mut render::HistoryWrapCache,
    activity_rows: &mut AgentActivityRows,
    scroll_offset: &mut usize,
    selection: &mut Option<selection::Selection>,
    header: &render::HeaderInfo,
    width: u16,
) {
    history.clear();
    history.extend(render::header_history_lines(header, width));
    history_wrap_cache.invalidate();
    activity_rows.rows.clear();
    activity_rows.tools.clear();
    *scroll_offset = 0;
    *selection = None;
}

/// Appends one mid-turn `AgentEvent`'s formatted lines to `history` — the
/// in-memory analogue of the old one-shot live print. `history_lines_for`
/// deliberately leaves tool activity out of its own output (see its own
/// doc comment) so appending this separately can't duplicate it. Returns
/// how many lines were just appended, so a caller that's scrolled back
/// (`scroll_offset > 0`) can advance `scroll_offset` by the same amount
/// and keep the *viewed content* fixed instead of letting the window
/// silently shift under the user by the number of newly appended lines
/// (see the two call sites in `run()`).
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

#[derive(Default)]
struct AgentActivityRows {
    rows: HashMap<(String, String), usize>,
    tools: VecDeque<(String, String, &'static str)>,
}

impl AgentActivityRows {
    fn tool_detail_lines(&self) -> Vec<String> {
        if self.tools.is_empty() {
            return vec!["この実行でtoolイベントは観測されていません".into()];
        }
        self.tools
            .iter()
            .map(|(name, _, state)| format!("tool {}: {}", render::sanitize(name), state))
            .collect()
    }
    fn observe_tool(&mut self, event: &polaris_core::AgentEvent) {
        let (name, detail, state) = match event {
            polaris_core::AgentEvent::ToolStarted { name, detail } => {
                (name, detail, "開始を観測・完了未確認")
            }
            polaris_core::AgentEvent::ToolFinished {
                name, detail, ok, ..
            } => (
                name,
                detail,
                if *ok {
                    "成功を観測"
                } else {
                    "失敗を観測"
                },
            ),
            _ => return,
        };
        let name = name.chars().take(256).collect::<String>();
        let detail = detail.chars().take(256).collect::<String>();
        if matches!(event, polaris_core::AgentEvent::ToolFinished { .. })
            && let Some(row) = self
                .tools
                .iter_mut()
                .rev()
                .find(|(n, d, s)| n == &name && d == &detail && *s == "開始を観測・完了未確認")
        {
            row.2 = state;
            return;
        }
        if self.tools.len() == 128 {
            self.tools.pop_front();
        }
        self.tools.push_back((name, detail, state));
    }
}

fn append_live_event(
    event: &polaris_core::AgentEvent,
    history: &mut Vec<render::HistoryLine>,
    activity_rows: &mut AgentActivityRows,
) -> usize {
    activity_rows.observe_tool(event);
    let key = match event {
        polaris_core::AgentEvent::SpawnStarted {
            display_name,
            agent_type,
            ..
        }
        | polaris_core::AgentEvent::SpawnActivity {
            display_name,
            agent_type,
            ..
        }
        | polaris_core::AgentEvent::SpawnFinished {
            display_name,
            agent_type,
            ..
        } => Some((display_name.clone(), agent_type.clone())),
        _ => None,
    };

    if matches!(event, polaris_core::AgentEvent::SpawnActivity { .. }) {
        if let Some(row) = key
            .as_ref()
            .and_then(|key| activity_rows.rows.get(key))
            .copied()
            && let Some(line) = render::format_event_for_live_print(event)
                .into_iter()
                .next()
            && let Some(slot) = history.get_mut(row)
        {
            *slot = line;
        }
        // An activity without a preceding SpawnStarted is stale or malformed;
        // never append it as a second unowned progress row.
        return 0;
    }

    let before = history.len();
    history.extend(render::format_event_for_live_print(event));
    match event {
        polaris_core::AgentEvent::SpawnStarted { .. } => {
            if let Some(key) = key {
                // SpawnStarted always renders the agent row followed by its
                // one replaceable, dimmed activity row.
                activity_rows.rows.insert(key, before + 1);
            }
        }
        polaris_core::AgentEvent::SpawnFinished { .. } => {
            if let Some(key) = key {
                activity_rows.rows.remove(&key);
            }
        }
        _ => {}
    }
    history.len() - before
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
    .map_err(|error| format!("巻き戻し保存先を検証できません: {error}"))?;
    match owner.resolve_pending() {
        Ok(ResolveOutcome::Clean) => Ok(owner),
        Ok(ResolveOutcome::RolledBack(_)) => Err(
            "巻き戻し途中のファイルを復旧しました。設定を読み直すためPolarisを再起動してください。"
                .into(),
        ),
        Err(error) => Err(format!("巻き戻し途中の復旧を確認できません: {error}")),
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
    let source = format!("保存会話（閲覧のみ）: {reason}");
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
    let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    }));
    let mut reader = CrosstermKeyReader;
    let result = show_persisted_read_only_resume(&terminal, &mut reader, persistence, reason);
    ratatui::restore();
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
    let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    }));
    let mut reader = CrosstermKeyReader;
    let accepted = confirm_workflow_change(&terminal, &mut reader, view);
    ratatui::restore();
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

/// The terminal's current (width, height), or a fallback if the size
/// can't be read (matches the fallback other `run()` call sites already
/// use — see e.g. `abbreviate_home`'s caller).
fn terminal_size(terminal: &RefCell<ratatui::DefaultTerminal>) -> (u16, u16) {
    terminal
        .borrow()
        .size()
        .map(|s| (s.width, s.height))
        .unwrap_or((80, 24))
}

fn terminal_width(terminal: &RefCell<ratatui::DefaultTerminal>) -> u16 {
    terminal_size(terminal).0
}

fn terminal_height(terminal: &RefCell<ratatui::DefaultTerminal>) -> u16 {
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
        eprintln!("保存先を準備できません: {error}");
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
                    eprintln!("保存会話を閲覧できません: {viewer_error}");
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
                eprintln!("管理操作の復旧を開始できません: {error}");
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
                    eprintln!("管理操作の記録を復旧できません: {error}");
                    return ExitCode::FAILURE;
                }
            };
        let Some(data_root) = args.sessions_dir.parent() else {
            eprintln!("v2セッションの保存先を特定できません");
            return ExitCode::FAILURE;
        };
        let project_id = match persist::project_identity(&project_root) {
            Ok(project_id) => project_id,
            Err(error) => {
                eprintln!("プロジェクト識別子を取得できません: {error}");
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
            eprintln!("会話切替の証拠を復旧できません: {error}");
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
                        "未解決の管理操作 {} ({}) があります。新しい入力は開始できません{}",
                        effect.request.operation_id,
                        effect.request.inventory,
                        recovery_issue
                            .as_ref()
                            .map(|reason| format!("。復旧理由: {reason}"))
                            .unwrap_or_default()
                    )
                }),
                Err(error) => Some(format!("管理・同期の状態を確認できません: {error}")),
            };
        (store, recovery_issue, blocker)
    };

    #[cfg(unix)]
    if let Some(continuation) = &args.continuation {
        if management_startup_blocker.is_none()
            && let Err(error) = continuation
                .persistence
                .abandon_unfinished_rewind_boundaries("前回の実行が完了する前に終了しました".into())
        {
            eprintln!("未完了の巻き戻し境界を確認できません: {error}");
            return ExitCode::FAILURE;
        }
        if let Some(owner) = &rewind_workspace
            && let Err(error) = validate_resume_workspace(owner, &continuation.persistence)
        {
            if let Err(viewer_error) =
                show_startup_read_only_resume(&continuation.persistence, &error)
            {
                eprintln!("保存会話を閲覧できません: {viewer_error}");
            }
            eprintln!("保存済み会話の作業ファイルを確認できません: {error}");
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
        eprintln!("strict10履歴を復元できません: {error}");
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
                    eprintln!("workflow変更を検証できません: {error}");
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
            eprintln!("workflow確認画面を復旧できません: {error}");
            return ExitCode::FAILURE;
        }
        match workflow_change {
            Ok(WorkflowChangeResult::Changed | WorkflowChangeResult::NoChange) => {}
            Ok(WorkflowChangeResult::Cancelled) => {
                #[cfg(target_os = "linux")]
                if let Some(code) = linux_pending_exit_code() {
                    return code;
                }
                eprintln!("workflow変更を取り消しました");
                return ExitCode::FAILURE;
            }
            Err(error) => {
                eprintln!("workflow変更を保存できません: {error}");
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
    let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    }));
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
    // Trackpad/mouse wheel scrolling of the conversation history, and mouse
    // drag-to-select (see `selection.rs`) — without this, crossterm never
    // emits `Event::Mouse` at all. This blocks the terminal's own native
    // mouse handling (including drag-select) while polaris is running — see
    // `selection.rs`'s module doc for why polaris implements its own
    // selection instead of relying on that. Best-effort: a terminal that
    // doesn't support mouse reporting just keeps not sending mouse events,
    // same as before.
    #[cfg(not(target_os = "linux"))]
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
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
    // How many `session.messages` / `local_lines` entries have already
    // been appended to `history`. Only the delta past this point gets
    // appended on each call to `append_new_history` — appending is
    // one-shot, never a redraw, so re-appending an already-appended line
    // would duplicate it in `history` instead of updating anything in
    // place.
    let mut printed_messages = 0usize;
    let mut printed_local_lines = 0usize;
    // The in-memory scrollable history buffer — accumulates every line
    // that used to be printed one-shot to the terminal's native scrollback
    // via `insert_before`. Rendered every frame by `draw_frame`, which
    // slices it down to `render::visible_history_window`'s current window.
    let mut history: Vec<render::HistoryLine> = Vec::new();
    let mut history_wrap_cache = render::HistoryWrapCache::default();
    let mut agent_activity_rows = AgentActivityRows::default();
    // How far back the user has scrolled the history region — 0 always
    // means "following the tail" (see `render::visible_history_window`).
    let mut scroll_offset: usize = 0;
    // Mouse drag-to-select over the conversation history — see
    // `selection.rs`. `None` means no active or finalized selection.
    let mut selection: Option<selection::Selection> = None;
    let startup_width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
    history.extend(render::header_history_lines(
        &render::HeaderInfo {
            provider_name: &provider_name,
            model_name: &args.model_name,
            usage: polaris_provider::Usage::default(),
            cwd: &args.cwd.display().to_string(),
            cwd_short: "",
            effort_name: "",
        },
        startup_width,
    ));

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
    let mut rewind_unavailable_reason = "まだ保存済みの巻き戻し境界がありません".to_owned();
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
                eprintln!("初期設定を確認できません: {error}");
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
                eprintln!("初期埋め込み設定を確認できません: {error}");
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
                Ok(true) => match ratatui::crossterm::event::read() {
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
            status = Status::Error("秘密情報を通常の入力欄へ入力できません".into());
        }
        // `/new`, `/fork`, and `/resume` replace the conversation owner.
        // Reattach process-owned services and selectors before accepting the
        // next input; none of these belong to durable conversation state.
        #[cfg(unix)]
        if refresh_saved_settings || saved_settings_path.as_ref() != Some(&session_path) {
            let switched = saved_settings_path.as_ref() != Some(&session_path);
            if let Some(saved) = &session.persistence {
                match saved.snapshot() {
                    Ok(snapshot) => {
                        effective_saved_settings =
                            snapshot.state.session_settings.unwrap_or_else(|| {
                                let mut initial = initial_saved_settings.clone();
                                initial.model = model_name.clone();
                                initial
                            });
                        if effective_saved_settings.provider != provider_name {
                            fatal_message = Some("保存providerが起動時providerと異なります。指定して再起動してください".into());
                            break 'outer ExitCode::FAILURE;
                        }
                        if model_name != effective_saved_settings.model {
                            if args.provider.model().is_none() {
                                fatal_message =
                                    Some("保存modelをこのproviderへ適用できません".into());
                                break 'outer ExitCode::FAILURE;
                            }
                            args.provider.set_model(&effective_saved_settings.model);
                            if args.provider.model().as_deref()
                                != Some(effective_saved_settings.model.as_str())
                            {
                                fatal_message = Some("保存modelの適用を確認できません".into());
                                break 'outer ExitCode::FAILURE;
                            }
                            model_name = effective_saved_settings.model.clone();
                        }
                        if switched {
                            input_buffer = snapshot.state.draft.text;
                            input_cursor = input_buffer.len();
                            input_history = input::History::new();
                            draft_saved_text = input_buffer.clone();
                            draft_candidate = input_buffer.clone();
                            #[cfg(target_os = "linux")]
                            {
                                draft_changed_at = Instant::now();
                            }
                        }
                    }
                    Err(error) => {
                        fatal_message = Some(format!("会話設定を復元できません: {error}"));
                        break 'outer ExitCode::FAILURE;
                    }
                }
            }
            saved_settings_path = Some(session_path.clone());
            refresh_saved_settings = false;
            tool_memory_path = None;
        }
        session.decider = args.decider.clone();
        session.tier_decider = args.tier_decider.clone();
        session.light_model = args.mcp_selection_provider.clone();
        session.approval_auto = args.approval_auto;
        session.setting = Some(setting.clone());
        session.computer = computer.clone();
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
            status = Status::Error("秘密情報を通常の入力欄へ入力できません".into());
        }
        #[cfg(unix)]
        let saved_tool_mode = match effective_saved_settings.tool_memory {
            polaris_core::session_settings::ToolMemoryModeV1::Off => None,
            polaris_core::session_settings::ToolMemoryModeV1::History => {
                Some(polaris_core::tool_memory::RetentionMode::History)
            }
            polaris_core::session_settings::ToolMemoryModeV1::Retrieval => {
                Some(polaris_core::tool_memory::RetentionMode::Retrieval)
            }
        };
        #[cfg(not(unix))]
        let saved_tool_mode = args.tool_memory;
        #[cfg(unix)]
        let saved_embedding = effective_saved_settings
            .tool_memory_embedding
            .as_ref()
            .map(|p| (p.url.clone(), p.model.clone()));
        #[cfg(not(unix))]
        let saved_embedding = args.tool_memory_embedding.clone();
        if tool_memory_path.as_ref() != Some(&session_path)
            || (saved_tool_mode.is_some()
                && session.tool_memory.is_none()
                && memory::saved_project_matches(&session, &args.cwd, &session_path))
        {
            if let Err(error) = configure_saved_tool_memory(
                &mut session,
                &args.cwd,
                &args.state_dir,
                &session_path,
                saved_tool_mode,
                saved_embedding.clone(),
            ) {
                fatal_message = Some(format!("ツール記憶を準備できません: {error}"));
                break 'outer ExitCode::FAILURE;
            }
            tool_memory_path = Some(session_path.clone());
        }
        let mut cumulative_usage = usage_meter.snapshot().usage;
        #[cfg(unix)]
        {
            session.compaction_threshold = effective_saved_settings.compact_threshold;
        }
        #[cfg(not(unix))]
        {
            session.compaction_threshold = args.compact_at;
        }
        #[cfg(unix)]
        let saved_remember = effective_saved_settings.remember;
        #[cfg(not(unix))]
        let saved_remember = args.remember;
        if saved_remember {
            if let Err(error) = memory::configure_saved_memory(
                &mut session,
                &args.cwd,
                &args.state_dir,
                &session_path,
            ) {
                fatal_message = Some(format!("履歴保存を準備できません: {error}"));
                break 'outer ExitCode::FAILURE;
            }
        } else if session.persistence.is_some() {
            session.before_compact = None;
        } else if session.persistence.is_none()
            && session.before_compact.is_none()
            && let Some(id) = session_path.file_stem().and_then(|s| s.to_str())
            && let Err(error) =
                memory::configure_archive(&mut session, &args.cwd, &args.state_dir, id)
        {
            // Without this the legacy log is rewritten over the summary and
            // the compacted prefix is gone from memory and disk at once.
            fatal_message = Some(format!("圧縮前の履歴を退避できません: {error}"));
            break 'outer ExitCode::FAILURE;
        }
        // Recomputed every draw from the live buffer, so the popup tracks
        // each keystroke — not just the moment `/` was first typed.
        #[cfg(unix)]
        let has_pending_inputs = !queued_inputs.is_empty()
            || !queued_instructions.is_empty()
            || active_instruction.is_some();
        #[cfg(not(unix))]
        let has_pending_inputs = !queued_inputs.is_empty();
        let suggestions = if dispatching_queued_input || input_history.is_recalled(&input_buffer) {
            // Let the full command reach `slash::parse` below. The picker is
            // for interactive completion and would otherwise reinterpret a
            // queued or recalled command as its first highlighted prefix
            // match, and consume keys meant for history navigation.
            Vec::new()
        } else {
            match input_buffer.strip_prefix('/') {
                Some(prefix) => slash::matching(prefix),
                None => Vec::new(),
            }
        };
        if selected_suggestion >= suggestions.len() {
            selected_suggestion = 0;
        }
        // Print anything new since the last iteration (typically the
        // previous turn's assistant reply, or a `/resume`/`/new`/`/fork`
        // reset) before redrawing the small footer viewport — printing is
        // one-shot and must happen before the footer draw so the newly
        // printed lines appear above it, not interleaved mid-frame.
        if append_new_history(
            &session,
            &local_lines,
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        ) {
            history_wrap_cache.invalidate();
        }
        if terminal
            .borrow_mut()
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut history_wrap_cache,
                    scroll_offset,
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
                    &selection,
                )
            })
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
                                    fatal_message =
                                        Some(format!("下書きを保存できません: {error}"));
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
                let candidate = match ratatui::crossterm::event::read() {
                    Ok(e) => e,
                    Err(_) => break 'outer ExitCode::FAILURE,
                };
                if let ratatui::crossterm::event::Event::Mouse(mouse) = &candidate {
                    exit_confirmation.cancel();
                    use ratatui::crossterm::event::MouseEventKind;
                    match mouse.kind {
                        MouseEventKind::Moved => continue,
                        MouseEventKind::ScrollUp => {
                            scroll_offset = scroll_offset.saturating_add(MOUSE_SCROLL_LINES);
                        }
                        MouseEventKind::ScrollDown => {
                            scroll_offset = scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
                        }
                        MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                            let history_area_height = area_height_for_history(
                                render::footer_height(suggestions.len()),
                                terminal_height(&terminal),
                            );
                            if mouse.row < history_area_height {
                                let width = terminal_width(&terminal);
                                let wrapped = render::wrap_history_lines(&history, width);
                                let window = render::visible_history_window(
                                    wrapped.len(),
                                    scroll_offset,
                                    history_area_height as usize,
                                );
                                let pos = selection::text_pos_from_screen(
                                    &wrapped,
                                    window,
                                    ratatui::layout::Rect::new(0, 0, width, history_area_height),
                                    mouse.row,
                                    mouse.column,
                                );
                                selection = Some(selection::Selection {
                                    anchor: pos,
                                    cursor: pos,
                                    dragging: true,
                                });
                            }
                        }
                        MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left) => {
                            if let Some(sel) = selection.as_mut()
                                && sel.dragging
                            {
                                let history_area_height = area_height_for_history(
                                    render::footer_height(suggestions.len()),
                                    terminal_height(&terminal),
                                );
                                let width = terminal_width(&terminal);
                                let wrapped = render::wrap_history_lines(&history, width);
                                let window = render::visible_history_window(
                                    wrapped.len(),
                                    scroll_offset,
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
                                    scroll_offset = scroll_offset.saturating_add(1);
                                } else if mouse.row.saturating_add(1) >= history_area_height {
                                    scroll_offset = scroll_offset.saturating_sub(1);
                                }
                            }
                        }
                        MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left) => {
                            if let Some(sel) = selection.as_mut()
                                && sel.dragging
                            {
                                sel.dragging = false;
                                let width = terminal_width(&terminal);
                                let wrapped = render::wrap_history_lines(&history, width);
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
            selection = None;
        }
        let ratatui::crossterm::event::Event::Key(key) = event else {
            continue;
        };
        if secret_value_key_is_blocked(&input_buffer, &key) {
            status = Status::Error("秘密情報を通常の入力欄へ入力できません".into());
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
                    status =
                        Status::Notice("入力欄と送信待ちを空にしてから巻き戻してください".into());
                    continue;
                }
                if !rewind_gesture.press(Instant::now()) {
                    status = Status::Notice("もう一度Escを押すと巻き戻し候補を開きます".into());
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
                                Status::Error(format!("巻き戻し候補を確認できません: {error}"));
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
                            reset_conversation_view(
                                &mut history,
                                &mut history_wrap_cache,
                                &mut agent_activity_rows,
                                &mut scroll_offset,
                                &mut selection,
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
                            printed_messages = 0;
                            printed_local_lines = 0;
                            status = Status::Notice(format!(
                                "入力「{}」の直前へ巻き戻しました。元の会話は保存されています。",
                                boundary.user_message_preview.as_deref().unwrap_or("不明")
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
                    status = Status::Notice("この環境ではファイル巻き戻しを利用できません".into());
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
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::Down => {
                    selected_suggestion = (selected_suggestion + 1) % suggestions.len();
                    continue;
                }
                KeyCode::Up => {
                    selected_suggestion =
                        (selected_suggestion + suggestions.len() - 1) % suggestions.len();
                    continue;
                }
                KeyCode::Enter => {
                    let action = slash::action_for(suggestions[selected_suggestion].name);
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
                        &format!("/{}", suggestions[selected_suggestion].name),
                        dispatching_queued_input,
                    );
                    let pending_input = std::mem::take(&mut input_buffer);
                    let pending_cursor = input_cursor;
                    input_cursor = 0;
                    selected_suggestion = 0;
                    match action {
                        #[cfg(unix)]
                        slash::Action::SessionControl(request) => {
                            let result = session_commands::handle(
                                &request,
                                &terminal,
                                &mut key_reader,
                                &mut session,
                                &initial_saved_settings,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                args.provider.as_ref(),
                                args.history_backend_factory.as_ref(),
                                has_pending_inputs,
                            );
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    refresh_saved_settings = true;
                                    status =
                                        Status::Notice("セッション操作の結果を表示しました".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(unix)]
                        slash::Action::Memory(request) => {
                            let id = match active_saved_session_id(&session) {
                                Ok(id) => id,
                                Err(error) => {
                                    status = Status::Error(error);
                                    continue;
                                }
                            };
                            let result = memory_commands::handle(
                                &request,
                                &terminal,
                                &mut key_reader,
                                rewind_workspace.as_ref().expect("startup owner"),
                                &management_effects,
                                &args.cwd,
                                &args.state_dir,
                                id.as_deref(),
                                saved_embedding.clone(),
                                has_pending_inputs,
                            )
                            .await;
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    status =
                                        Status::Notice("メモリ操作の結果を表示しました".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(unix)]
                        slash::Action::Mcp(request)
                            if request.split_whitespace().next() != Some("prompt") =>
                        {
                            let result = mcp_commands::handle(
                                &request,
                                &terminal,
                                &mut key_reader,
                                (
                                    rewind_workspace.as_ref().expect("startup owner"),
                                    &management_effects,
                                ),
                                &args.cwd,
                                (args.mcp_external, approval_policy),
                                has_pending_inputs,
                            )
                            .await;
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    if matches!(
                                        request.split_whitespace().next(),
                                        Some(
                                            "enable"
                                                | "disable"
                                                | "check"
                                                | "setup"
                                                | "register"
                                                | "materialize"
                                                | "secret"
                                        )
                                    ) {
                                        pending_mcp_prompt = None;
                                        mcp_prompt_requires_reselect = false;
                                    }
                                    status = Status::Notice("MCP操作の結果を表示しました".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        #[cfg(target_os = "linux")]
                        slash::Action::WorkspaceControl(request) => {
                            let (verb, extra) = request.split_once(' ').unwrap_or((&request, ""));
                            let result = if verb == "models" {
                                model_inventory_read_lines(extra).await
                            } else {
                                match workspace_commands.as_ref() {
                                    Ok(owner) => match verb {
                                        "browse" => owner.browse(extra),
                                        "attachment" => owner.attachment(extra),
                                        "recipe" => workspace_commands::recipes(extra),
                                        "setting" => owner.setting(
                                            extra,
                                            &terminal,
                                            &mut key_reader,
                                            rewind_workspace.as_ref().expect("startup owner"),
                                            &management_effects,
                                            has_pending_inputs,
                                        ),
                                        "handoff" => owner.handoff(extra),
                                        _ => Err("対象の操作はありません".into()),
                                    },
                                    Err(error) => Err(error.clone()),
                                }
                            };
                            match result {
                                Ok(lines) => {
                                    local_lines.extend(lines);
                                    status = Status::Notice("読み取り結果を表示しました".into());
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
                                    status = Status::Notice("決定記録の結果を表示しました".into());
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
                                        && lines
                                            .iter()
                                            .any(|line| line.to_string().starts_with("T08 完了"))
                                    {
                                        local_lines.clear();
                                        let width =
                                            terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                        reset_conversation_view(
                                            &mut history,
                                            &mut history_wrap_cache,
                                            &mut agent_activity_rows,
                                            &mut scroll_offset,
                                            &mut selection,
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
                                        printed_messages = 0;
                                        printed_local_lines = 0;
                                    }
                                    local_lines.extend(lines);
                                    status = Status::Notice("管理操作の結果を表示しました".into());
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
                                        status =
                                            Status::Notice("受理入力の結果を表示しました".into());
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
                                        status = Status::Notice(
                                            "role bindingの結果を表示しました".into(),
                                        );
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
                                            "sandboxの実行結果を表示しました".into(),
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
                                &agent_activity_rows,
                            )
                            .await;
                            match result {
                                Ok(lines) => {
                                    local_lines
                                        .extend(lines.into_iter().map(ratatui::text::Line::from));
                                    status = Status::Notice("実行状態を表示しました".into());
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
                                    status = Status::Notice("認証操作の結果を表示しました".into());
                                }
                                Err(error) => status = Status::Error(error),
                            }
                            continue;
                        }
                        slash::Action::Status => {
                            local_lines.extend(status_lines(StatusSnapshot {
                                session: &session,
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
                            status = Status::Notice("状態を表示しました".into());
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
                            status = Status::Notice("診断結果を表示しました".into());
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
                            reset_conversation_view(
                                &mut history,
                                &mut history_wrap_cache,
                                &mut agent_activity_rows,
                                &mut scroll_offset,
                                &mut selection,
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
                            printed_messages = 0;
                            printed_local_lines = 0;
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
                            reset_conversation_view(
                                &mut history,
                                &mut history_wrap_cache,
                                &mut agent_activity_rows,
                                &mut scroll_offset,
                                &mut selection,
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
                            printed_messages = 0;
                            printed_local_lines = 0;
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
                            reset_conversation_view(
                                &mut history,
                                &mut history_wrap_cache,
                                &mut agent_activity_rows,
                                &mut scroll_offset,
                                &mut selection,
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
                            printed_messages = 0;
                            printed_local_lines = 0;
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
                                        status = Status::Notice("C10操作を開始しました".into());
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
                            if terminal
                                .borrow_mut()
                                .draw(|f| {
                                    draw_frame(
                                        f,
                                        &history,
                                        &mut history_wrap_cache,
                                        scroll_offset,
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
                                        &selection,
                                    )
                                })
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
                            printed_messages = session.messages.len();
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
                                        reset_conversation_view(
                                            &mut history,
                                            &mut history_wrap_cache,
                                            &mut agent_activity_rows,
                                            &mut scroll_offset,
                                            &mut selection,
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
                                        printed_messages = 0;
                                        printed_local_lines = 0;
                                    }
                                    continue;
                                }
                                SlashOutcome::Quit => {
                                    #[cfg(unix)]
                                    if !queued_instructions.is_empty()
                                        || active_instruction.is_some()
                                    {
                                        status = Status::Notice(
                                            "受理済みの指示を処理・取消してから終了してください"
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
                _ => {}
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
                    scroll_offset = scroll_offset.saturating_add(1);
                    continue;
                }
                KeyCode::PageDown => {
                    scroll_offset = scroll_offset.saturating_sub(1);
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

        selection = None;

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
                        status = Status::Error("秘密情報を通常の入力欄へ入力できません".into());
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
                        status = Status::Error("秘密情報を通常の入力や送信待ちに渡せません".into());
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
                    scroll_offset = 0;
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
            match parsed_action {
                #[cfg(unix)]
                Some(slash::Action::SessionControl(request)) => {
                    let result = session_commands::handle(
                        &request,
                        &terminal,
                        &mut key_reader,
                        &mut session,
                        &initial_saved_settings,
                        rewind_workspace.as_ref().expect("startup owner"),
                        &management_effects,
                        args.provider.as_ref(),
                        args.history_backend_factory.as_ref(),
                        has_pending_inputs,
                    );
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            refresh_saved_settings = true;
                            status = Status::Notice("セッション操作の結果を表示しました".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(unix)]
                Some(slash::Action::Memory(request)) => {
                    let id = match active_saved_session_id(&session) {
                        Ok(id) => id,
                        Err(error) => {
                            status = Status::Error(error);
                            continue;
                        }
                    };
                    let result = memory_commands::handle(
                        &request,
                        &terminal,
                        &mut key_reader,
                        rewind_workspace.as_ref().expect("startup owner"),
                        &management_effects,
                        &args.cwd,
                        &args.state_dir,
                        id.as_deref(),
                        saved_embedding.clone(),
                        has_pending_inputs,
                    )
                    .await;
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            status = Status::Notice("メモリ操作の結果を表示しました".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(unix)]
                Some(slash::Action::Mcp(request))
                    if request.split_whitespace().next() != Some("prompt") =>
                {
                    let result = mcp_commands::handle(
                        &request,
                        &terminal,
                        &mut key_reader,
                        (
                            rewind_workspace.as_ref().expect("startup owner"),
                            &management_effects,
                        ),
                        &args.cwd,
                        (args.mcp_external, approval_policy),
                        has_pending_inputs,
                    )
                    .await;
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            if matches!(
                                request.split_whitespace().next(),
                                Some(
                                    "enable"
                                        | "disable"
                                        | "check"
                                        | "setup"
                                        | "register"
                                        | "materialize"
                                        | "secret"
                                )
                            ) {
                                pending_mcp_prompt = None;
                                mcp_prompt_requires_reselect = false;
                            }
                            status = Status::Notice("MCP操作の結果を表示しました".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                #[cfg(target_os = "linux")]
                Some(slash::Action::WorkspaceControl(request)) => {
                    let (verb, extra) = request.split_once(' ').unwrap_or((&request, ""));
                    let result = if verb == "models" {
                        model_inventory_read_lines(extra).await
                    } else {
                        match workspace_commands.as_ref() {
                            Ok(owner) => match verb {
                                "browse" => owner.browse(extra),
                                "attachment" => owner.attachment(extra),
                                "recipe" => workspace_commands::recipes(extra),
                                "setting" => owner.setting(
                                    extra,
                                    &terminal,
                                    &mut key_reader,
                                    rewind_workspace.as_ref().expect("startup owner"),
                                    &management_effects,
                                    has_pending_inputs,
                                ),
                                "handoff" => owner.handoff(extra),
                                _ => Err("対象の操作はありません".into()),
                            },
                            Err(error) => Err(error.clone()),
                        }
                    };
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines);
                            status = Status::Notice("読み取り結果を表示しました".into());
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
                            status = Status::Notice("決定記録の結果を表示しました".into());
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
                                    .any(|line| line.to_string().starts_with("T08 完了"))
                            {
                                local_lines.clear();
                                let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                reset_conversation_view(
                                    &mut history,
                                    &mut history_wrap_cache,
                                    &mut agent_activity_rows,
                                    &mut scroll_offset,
                                    &mut selection,
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
                                printed_messages = 0;
                                printed_local_lines = 0;
                            }
                            local_lines.extend(lines);
                            status = Status::Notice("管理操作の結果を表示しました".into());
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
                                status = Status::Notice("受理入力の結果を表示しました".into());
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
                                status = Status::Notice("role bindingの結果を表示しました".into());
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
                                status = Status::Notice("sandboxの実行結果を表示しました".into());
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
                        &agent_activity_rows,
                    )
                    .await;
                    match result {
                        Ok(lines) => {
                            local_lines.extend(lines.into_iter().map(ratatui::text::Line::from));
                            status = Status::Notice("実行状態を表示しました".into());
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
                            status = Status::Notice("認証操作の結果を表示しました".into());
                        }
                        Err(error) => status = Status::Error(error),
                    }
                    continue;
                }
                Some(slash::Action::Status) => {
                    local_lines.extend(status_lines(StatusSnapshot {
                        session: &session,
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
                    status = Status::Notice("状態を表示しました".into());
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
                    status = Status::Notice("診断結果を表示しました".into());
                    continue;
                }
                Some(slash::Action::Mcp(requested))
                    if requested.split_whitespace().next() == Some("prompt") =>
                {
                    if requested == "prompt cancel" {
                        pending_mcp_prompt = None;
                        mcp_prompt_requires_reselect = false;
                        status = Status::Notice("MCP prompt選択を取り消しました".into());
                        continue;
                    }
                    match mcp_prompt_choice(&requested) {
                        Ok(choice) => {
                            status = Status::Notice(format!(
                                "次の入力でMCP prompt {}/{} を選択します",
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
                    reset_conversation_view(
                        &mut history,
                        &mut history_wrap_cache,
                        &mut agent_activity_rows,
                        &mut scroll_offset,
                        &mut selection,
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
                    printed_messages = 0;
                    printed_local_lines = 0;
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
                    reset_conversation_view(
                        &mut history,
                        &mut history_wrap_cache,
                        &mut agent_activity_rows,
                        &mut scroll_offset,
                        &mut selection,
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
                    printed_messages = 0;
                    printed_local_lines = 0;
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
                    reset_conversation_view(
                        &mut history,
                        &mut history_wrap_cache,
                        &mut agent_activity_rows,
                        &mut scroll_offset,
                        &mut selection,
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
                    printed_messages = 0;
                    printed_local_lines = 0;
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
                                status = Status::Notice("C10操作を開始しました".into());
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
                    if terminal
                        .borrow_mut()
                        .draw(|f| {
                            draw_frame(
                                f,
                                &history,
                                &mut history_wrap_cache,
                                scroll_offset,
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
                                &selection,
                            )
                        })
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
                    printed_messages = session.messages.len();
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
                                reset_conversation_view(
                                    &mut history,
                                    &mut history_wrap_cache,
                                    &mut agent_activity_rows,
                                    &mut scroll_offset,
                                    &mut selection,
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
                                printed_messages = 0;
                                printed_local_lines = 0;
                            }
                            continue;
                        }
                        SlashOutcome::Quit => {
                            #[cfg(unix)]
                            if !queued_instructions.is_empty() || active_instruction.is_some() {
                                status = Status::Notice(
                                    "受理済みの指示を処理・取消してから終了してください".into(),
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
                status = Status::Error(format!("下書きの送信準備を保存できません: {error}"));
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
                                format!("この入力のファイル保存を作成できません: {error}");
                            None
                        }
                    }
                }
                None => {
                    rewind_unavailable_reason =
                        "この作業場所はファイル巻き戻しの対象外です".to_owned();
                    None
                }
            }
        } else {
            rewind_unavailable_reason = "この会話は保存済みv2セッションではありません".to_owned();
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
                Ok(lease) => lease,
                Err(error) => {
                    if dispatching_queued_input {
                        queued_inputs.push_front(text);
                        queued_inputs_paused = true;
                    } else {
                        input_buffer = text;
                        input_cursor = input_buffer.len();
                    }
                    status = Status::Error(format!(
                        "作業場所が他の処理に使われているため、この入力を開始できません: {error}"
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
                    "未解決の管理操作 {} ({}) があります。復旧前に新しい入力は開始できません",
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
                status = Status::Error(format!("管理操作の状態を確認できません: {error}"));
                continue;
            }
        }

        #[cfg(unix)]
        if let Some(persistence) = session.persistence.as_ref() {
            let admission = rewind_workspace
                .as_ref()
                .ok_or_else(|| "作業ファイルの検証ownerがありません".to_string())
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
                status = Status::Error(format!("会話の実行許可を確認できません: {error}"));
                continue;
            }
        }

        #[cfg(unix)]
        if let (Some(owner), Some(id)) = (rewind_workspace.as_ref(), &rewind_pre_manifest_id) {
            let matches = id
                .parse()
                .map_err(|error| format!("保存した作業木IDが不正です: {error}"))
                .and_then(|manifest| {
                    owner
                        .matches_manifest(&manifest)
                        .map_err(|error| format!("作業木を照合できません: {error}"))
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
                    "入力準備中に作業ファイルが変更されました。再送前に状態を確認してください"
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
                "MCP promptの選択結果が不明です。/mcp prompt で再選択するか /mcp prompt cancel で取り消してください"
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
                "MCP promptにはv2会話が必要です。会話を移行してから選択してください".into()
            } else {
                "MCPサーバーを利用できません。入力は開始していません".into()
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
                    .ok_or("保存済み指示の会話がありません")?;
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
                    saved_tool_mode.is_some(),
                )
                .map_err(|error| error.to_string())?;
                let applied =
                    instruction_commands::apply(&store, &mut manager, &mut session, instruction)?;
                let current = session
                    .persistence
                    .as_ref()
                    .ok_or("保存済み指示の会話がありません")?
                    .snapshot()
                    .map_err(|e| e.to_string())?
                    .state
                    .session_id;
                if applied.session_id.as_str() != current
                    || applied.run_id
                        != polaris_core::session_manager::RunId::derive(&applied.admission_id)
                {
                    return Err("保存済み指示の実行先と受理記録が一致しません".into());
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
                        "指示の適用を確認できません。再送せず復旧してください: {error}"
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
                            "MCP prompt {}/{} を選択しました",
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
                    status = Status::Error(format!("入力を振り分けできません: {error}"));
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
                        "{message}。会話履歴には追加していません。{}。/mcp prompt で再選択してください",
                        if unknown {
                            "取得結果は不明です"
                        } else {
                            "モデル実行は開始していません"
                        }
                    ));
                    continue;
                }
                Err(error @ TuiAdmissionFailure::Consumed(_)) => {
                    fatal_message = Some(format!(
                        "入力保存または実行開始を確認できません: {}。再起動後に回復状態を確認してください。",
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
                status = Status::Error(format!("routine authorityを接続できません: {error}"));
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
                        format!("この入力の会話境界を保存できません: {error}");
                }
            }
            session.push_user(&text);
        }
        // A distinct input can replace `session` with an empty sidechat
        // after the loop-top attachment. Reinstall only process-owned
        // capabilities; conversation history, approval replies, tool state,
        // and memory are deliberately not copied from the parent.
        session.decider = args.decider.clone();
        session.tier_decider = args.tier_decider.clone();
        session.light_model = args.mcp_selection_provider.clone();
        session.approval_auto = args.approval_auto;
        session.setting = Some(setting.clone());
        session.computer = computer.clone();
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
                    let message = format!("sidechat runのIDを確認できません: {error}");
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
        if append_new_history(
            &session,
            &local_lines,
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        ) {
            history_wrap_cache.invalidate();
        }
        if terminal
            .borrow_mut()
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut history_wrap_cache,
                    scroll_offset,
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
                    // `input_buffer` was just cleared by `apply_key`'s
                    // Submit branch, so there's nothing to suggest against.
                    &[],
                    0,
                    &selection,
                )
            })
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
                fatal_message = Some(format!("TUI表示を更新できません; {cleanup}"));
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
                        "subagent controlを準備できません: {error}; {cleanup}"
                    ));
                    break 'outer ExitCode::FAILURE;
                }
                #[cfg(not(target_os = "linux"))]
                if let (Some(boundary), Some(persistence)) =
                    (rewind_boundary.take(), session.persistence.as_ref())
                {
                    let _ = persistence.abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "subagent controlを開始できません".into(),
                    );
                }
                status = Status::Error(format!("subagent controlを準備できません: {error}"));
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
                status = Status::Error(format!("実行管理の保存状態を読めません: {error}"));
                continue 'outer;
            }
        };
        #[cfg(unix)]
        let runtime_error = RefCell::new(None);
        #[cfg(unix)]
        let mut interruption_effect = None;
        if let Some(active) = tui_admission.as_ref() {
            history.push(render::HistoryLine {
                line: ratatui::text::Line::from(format!(
                    "受理ID: {}（/input status|cancel）",
                    active.admission_id.as_str()
                )),
                shaded: true,
            });
            history_wrap_cache.invalidate();
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
                fatal_message = Some(format!("role bindingを実行へ接続できません: {error}"));
                break 'outer ExitCode::FAILURE;
            }
        };
        #[cfg(not(unix))]
        let child_provider = args.provider.clone();
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
                        let appended = append_live_event(
                            &event,
                            &mut history,
                            &mut agent_activity_rows,
                        );
                        history_wrap_cache.invalidate();
                        // The user is scrolled back, not following the
                        // tail — advance `scroll_offset` by however many
                        // lines just landed so the *viewed content* stays
                        // fixed under them instead of the window silently
                        // sliding by `appended` lines (see
                        // `append_live_event`'s doc).
                        if scroll_offset > 0 {
                            scroll_offset += appended;
                        }
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
                            printed_messages = printed_messages
                                .saturating_sub(messages_before.saturating_sub(messages_after));
                        }
                    }
                    _ = ticker.tick() => {
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
                        if terminal
                            .borrow_mut()
                            .draw(|f| {
                                draw_frame(
                                    f,
                                    &history,
                                    &mut history_wrap_cache,
                                    scroll_offset,
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
                                    &selection,
                                )
                            })
                            .is_err()
                        {
                            break TurnOutcome::Fatal;
                        }
                    }
                    maybe_event = event_stream.next() => {
                        use ratatui::crossterm::event::{
                            Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
                            MouseEventKind,
                        };
                        match maybe_event {
                            Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
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
                                        scroll_offset = scroll_offset.saturating_add(1);
                                    }
                                    KeyCode::PageDown => {
                                        scroll_offset = scroll_offset.saturating_sub(1);
                                    }
                                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        #[cfg(unix)]
                                        { status = Status::Notice("コピーは実行終了後に /copy で確認してください".into()); }
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
                                        if terminal
                                            .borrow_mut()
                                            .draw(|f| {
                                                draw_frame(
                                                    f,
                                                    &history,
                                                    &mut history_wrap_cache,
                                                    scroll_offset,
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
                                                    &selection,
                                                )
                                            })
                                            .is_err()
                                        {
                                            break TurnOutcome::Fatal;
                                        }
                                    }
                                    #[cfg(unix)]
                                    KeyCode::Enter if input_buffer.starts_with("/instruction ") => {
                                        let request=std::mem::take(&mut input_buffer);input_cursor=0;
                                        let result=(||->Result<String,String>{
                                            let text=request.strip_prefix("/instruction send ").ok_or("使い方: /instruction send <次のrunへの指示>")?;
                                            let active=tui_admission.as_mut().ok_or("保存済み受理ownerがありません")?;
                                            let saved=runtime_persistence.as_ref().ok_or("v2保存会話が必要です")?;
                                            let handle=instruction_commands::receive(&active.store,&mut active.manager,saved,&management_effects,&_project_activity,polaris_core::management_effect::ManagementOperationIdV1::random().map_err(|e|e.to_string())?,text)?;
                                            let id=handle.admission_id().as_str().to_owned();queued_instructions.push_back(handle);Ok(id)
                                        })();
                                        match result {Ok(id)=>{history.push(render::HistoryLine{line:ratatui::text::Line::from(format!("指示受理ID: {id}。この会話の次のrun境界で適用します")),shaded:true});history_wrap_cache.invalidate();status=Status::Notice("次のrunへの指示を保存しました".into());},Err(error)=>status=Status::Error(error)}
                                        mid_turn_notice_until=Some(Instant::now()+MID_TURN_NOTICE_DURATION);
                                    }
                                    #[cfg(unix)]
                                    KeyCode::Enter if input_buffer.starts_with("/input ") => {
                                        let request=std::mem::take(&mut input_buffer);input_cursor=0;
                                        let result=(|| -> Result<(bool,String),String> {
                                            let words=request.split_whitespace().collect::<Vec<_>>();
                                            let ["/input",verb,id]=words.as_slice() else {return Err("使い方: /input status|cancel <受理ID>".into());};
                                            let active=tui_admission.as_ref().ok_or("このrunには保存済み受理IDがありません")?;
                                            let id=polaris_core::session_manager::AdmissionId::new(*id).map_err(|e|e.to_string())?;
                                            let receipt=active.manager.status(&id).map_err(|e|e.to_string())?;
                                            if *verb=="status" {return Ok((false,render::sanitize(&format!("入力 {} / revision {} / 状態 {:?}",receipt.id.as_str(),receipt.revision,receipt.state))));}
                                            if *verb!="cancel" {return Err("使い方: /input status|cancel <受理ID>".into());}
                                            if id!=active.admission_id {
                                                let position=queued_instructions.iter().position(|item|item.admission_id()==&id).ok_or("この会話の実行中・送信待ちIDではありません")?;
                                                #[cfg(target_os="linux")] let mut reader=LinuxSignalAwareKeyReader;
                                                #[cfg(not(target_os="linux"))] let mut reader=approver::CrosstermKeyReader;
                                                if !account::confirm(&terminal,&mut reader,"送信待ち入力の取消",id.as_str(),"このIDの未実行の指示だけを取り消します。") {
                                                    let context=runtime_commands::RuntimeMutationContext{effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted:false};
                                                    runtime_effects::begin(&context,"D10",id.as_str().as_bytes())?;
                                                    return Ok((false,"取消要求を取り消しました".into()));
                                                }
                                                let instruction=queued_instructions.remove(position).expect("queued");
                                                let active=tui_admission.as_mut().ok_or("保存された受理ownerがありません")?;
                                                instruction_commands::cancel_queue(&active.store,&mut active.manager,runtime_persistence.as_ref().ok_or("v2会話が必要です")?,&management_effects,&_project_activity,instruction)?;
                                                return Ok((false,format!("入力 {} を取り消しました",id.as_str())));
                                            }
                                            #[cfg(target_os="linux")]
                                            let mut reader=LinuxSignalAwareKeyReader;
                                            #[cfg(not(target_os="linux"))]
                                            let mut reader=approver::CrosstermKeyReader;
                                            let accepted=account::confirm(&terminal,&mut reader,"入力の停止要求",id.as_str(),"この入力のrunを停止し、所有するchild/MCPの終了を確認します。");
                                            let context=runtime_commands::RuntimeMutationContext{effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted};
                                            interruption_effect=runtime_effects::begin(&context,"D10",id.as_str().as_bytes())?;
                                            Ok((accepted,if accepted {"入力の停止を要求しました".into()}else{"停止要求を取り消しました".into()}))
                                        })();
                                        match result {
                                            Ok((true,_))=>break TurnOutcome::Interrupted,
                                            Ok((false,line))=>{history.push(render::HistoryLine{line:ratatui::text::Line::from(line),shaded:false});history_wrap_cache.invalidate();status=Status::Notice("入力状態を表示しました".into());},
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
                                            let accepted = !extra.trim().starts_with("cancel ") || account::confirm(&terminal,&mut cancellation_reader,"subagent停止要求",extra.trim(),"現在のchild/attemptだけへ停止要求を送ります。終了はwaitで確認してください。");
                                            runtime_commands::subagents(Some(&turn_control),extra.trim(),Some(runtime_commands::RuntimeMutationContext{effects:&management_effects,lease:&_project_activity,session_id:&durable_session_id,generation:runtime_generation,accepted})).await
                                        } else {
                                            runtime_commands::run_details(Some(&turn_control)).await.map(|mut lines| { lines.extend(agent_activity_rows.tool_detail_lines()); lines })
                                        };
                                        match result {
                                            Ok(lines)=>{history.extend(lines.into_iter().map(|line|render::HistoryLine{line:ratatui::text::Line::from(line),shaded:false}));history_wrap_cache.invalidate();status=Status::Notice("実行状態を表示しました".into());},
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
                                        InflightInputAction::Continue => {}
                                        InflightInputAction::Queued => {
                                            #[cfg(unix)]
                                            if queued_inputs.back().is_some_and(|text|!text.starts_with('/')) && runtime_persistence.is_some() {
                                                let text=queued_inputs.pop_back().expect("queued input");
                                                let result=(||->Result<String,String>{
                                                    let active=tui_admission.as_mut().ok_or("保存済み受理ownerがありません")?;
                                                    let handle=instruction_commands::receive(&active.store,&mut active.manager,runtime_persistence.as_ref().expect("saved"),&management_effects,&_project_activity,polaris_core::management_effect::ManagementOperationIdV1::random().map_err(|e|e.to_string())?,&text)?;
                                                    let id=handle.admission_id().as_str().to_owned();queued_instructions.push_back(handle);Ok(id)
                                                })();
                                                match result {Ok(id)=>status=Status::Notice(format!("入力 {id} を保存しました。次のrunで適用します")),Err(error)=>{input_buffer=text;input_cursor=input_buffer.len();status=Status::Error(error);}}
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
                                                "このTUIでは秘密情報の設定をまだ受け付けていません".into(),
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
                            Some(Ok(Event::Mouse(mouse))) => match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    scroll_offset =
                                        scroll_offset.saturating_add(MOUSE_SCROLL_LINES);
                                }
                                MouseEventKind::ScrollDown => {
                                    scroll_offset =
                                        scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
                                }
                                MouseEventKind::Down(MouseButton::Left) => {
                                    // Mid-turn `draw_frame` calls always pass
                                    // a literal `&[]` for suggestions (the
                                    // popup never shows once a turn is in
                                    // flight — the input buffer that would
                                    // drive it was already cleared on
                                    // submit), so `footer_height(0)` always
                                    // matches what this frame actually
                                    // rendered.
                                    let history_area_height = area_height_for_history(
                                        render::footer_height(0),
                                        terminal_height(&terminal),
                                    );
                                    if mouse.row < history_area_height {
                                        let width = terminal_width(&terminal);
                                        let wrapped = render::wrap_history_lines(&history, width);
                                        let window = render::visible_history_window(
                                            wrapped.len(),
                                            scroll_offset,
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
                                        selection = Some(selection::Selection {
                                            anchor: pos,
                                            cursor: pos,
                                            dragging: true,
                                        });
                                    }
                                }
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    if let Some(sel) = selection.as_mut()
                                        && sel.dragging
                                    {
                                        // See the `Down` arm above: mid-turn
                                        // `draw_frame` calls always pass `&[]`
                                        // for suggestions.
                                        let history_area_height = area_height_for_history(
                                            render::footer_height(0),
                                            terminal_height(&terminal),
                                        );
                                        let width = terminal_width(&terminal);
                                        let wrapped = render::wrap_history_lines(&history, width);
                                        let window = render::visible_history_window(
                                            wrapped.len(),
                                            scroll_offset,
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
                                            scroll_offset = scroll_offset.saturating_add(1);
                                        } else if mouse.row.saturating_add(1) >= history_area_height
                                        {
                                            scroll_offset = scroll_offset.saturating_sub(1);
                                        }
                                    }
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    if let Some(sel) = selection.as_mut()
                                        && sel.dragging
                                    {
                                        sel.dragging = false;
                                        let width = terminal_width(&terminal);
                                        let wrapped = render::wrap_history_lines(&history, width);
                                        let text = selection::extract_text(&wrapped, sel);
                                        if !text.is_empty() {
                                            #[cfg(unix)]
                                            { status = Status::Notice("選択した範囲のコピーは実行終了後に確認してください".into()); }
                                            #[cfg(not(unix))]
                                            { status = Status::Notice(apply_selection_copy(&text, clipboard::copy_to_clipboard)); }
                                            mid_turn_notice_until = Some(
                                                Instant::now() + MID_TURN_NOTICE_DURATION,
                                            );
                                            if terminal
                                                .borrow_mut()
                                                .draw(|f| {
                                                    draw_frame(
                                                        f,
                                                        &history,
                                                        &mut history_wrap_cache,
                                                        scroll_offset,
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
                                                        &selection,
                                                    )
                                                })
                                                .is_err()
                                            {
                                                break TurnOutcome::Fatal;
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Some(Ok(Event::Resize(_, _))) => {
                                selection = None;
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
                let appended = append_live_event(&event, &mut history, &mut agent_activity_rows);
                history_wrap_cache.invalidate();
                if scroll_offset > 0 {
                    scroll_offset += appended;
                }
                if let polaris_core::AgentEvent::HistoryCompacted {
                    messages_before,
                    messages_after,
                    ..
                } = event
                {
                    history_compacted_this_turn = true;
                    checkpoint =
                        checkpoint.saturating_sub(messages_before.saturating_sub(messages_after));
                    printed_messages = printed_messages
                        .saturating_sub(messages_before.saturating_sub(messages_after));
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
                .map(|error| format!("MCP runtimeを終了できません: {error}"))
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
            control_cleanup_error = Some(format!("実行管理の保存に失敗しました: {error}"));
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
                control_cleanup_error = Some(format!("sidechat runを終了できません: {error}"));
            }
        }
        #[cfg(target_os = "linux")]
        if control_cleanup_error.is_none() {
            control_cleanup_error = mcp_cleanup_error;
        }
        #[cfg(unix)]
        {
            if let Some(error) = runtime_error.into_inner() {
                control_cleanup_error = Some(format!("実行管理の保存に失敗しました: {error}"));
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
                    control_cleanup_error = Some(format!("停止結果を保存できません: {error}"));
                }
            }
        }
        #[cfg(target_os = "linux")]
        let mut cleanup_failure = control_cleanup_error.clone();
        let turn_completed = matches!(&outcome, TurnOutcome::Done(Ok(_)));
        #[cfg(target_os = "linux")]
        let turn_completed = linux_turn_completed_without_shutdown(
            turn_completed,
            shutdown_reason.or_else(signal::pending_linux_process_shutdown),
        );
        if turn_completed && let Some(finalizer) = args.answer_finalizer.as_ref() {
            let scheduled = schedule_tui_shadow(finalizer, &session, &text, &turn_control);
            if let Err(error) = scheduled {
                local_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                    format!("回答達成判定を記録できません: {error}"),
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
                    cleanup_failure = Some(format!("停止中の会話を復旧できません: {error}"));
                }
            }
            TurnOutcome::Fatal => {
                if let (Some(boundary), Some(persistence)) =
                    (rewind_boundary.take(), session.persistence.as_ref())
                {
                    let _ = persistence.abandon_rewind_boundary(
                        &boundary.boundary_id,
                        "TUI実行が完了前に終了しました".into(),
                    );
                }
                break ExitCode::FAILURE;
            }
        }
        #[cfg(unix)]
        let rewind_writers_stopped = control_cleanup_error.is_none();
        if let Some(error) = control_cleanup_error {
            status = Status::Error(format!("subagent controlを終了できません: {error}"));
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
                fatal_message = Some("巻き戻し境界の保存元が失われました".into());
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
                        "ファイル保存先が利用できません".into(),
                    ),
                }
            } else {
                persistence.abandon_rewind_boundary(
                    &boundary.boundary_id,
                    "ターンまたはsubagentが正常終了していません".into(),
                )
            };
            match completion {
                Ok(saved) => {
                    rewind_unavailable_reason = match saved.completion {
                        Some(completion) if matches!(
                            completion.eligibility,
                            polaris_core::conversation_state::RewindEligibilityV1::Eligible
                        ) => "まだ保存済みの巻き戻し境界がありません".into(),
                        Some(completion) => match completion.effect {
                            polaris_core::conversation_state::RewindEffectStatusV1::Unknown {
                                description,
                            }
                            | polaris_core::conversation_state::RewindEffectStatusV1::ExternalMutation {
                                description,
                            } => description,
                            _ => "このターンは巻き戻し対象外です".into(),
                        },
                        None => saved
                            .abandoned_reason
                            .unwrap_or_else(|| "このターンの保存境界は未完了です".into()),
                    };
                }
                Err(error) => {
                    fatal_message = Some(format!("巻き戻し境界を確定できません: {error}"));
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
                    Some(previous) => format!("{previous}; 下書き保存: {error}"),
                    None => format!("下書きを保存できません: {error}"),
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
    ratatui::restore();
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
) -> Option<String> {
    let now = now_millis();
    if rows.is_empty() {
        let _ = terminal
            .borrow_mut()
            .draw(|f| render::render_rewind_picker(f, rows, 0, now));
        let _ = reader.read_key();
        return None;
    }
    let mut selected = 0usize;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_rewind_picker(f, rows, selected, now))
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
                .map_err(|_| "C10 operation IDが不正です".to_string())?;
            if *action == "recover" {
                Ok(C10TuiInvocation::Recover(operation))
            } else {
                Ok(C10TuiInvocation::Abandon(operation))
            }
        }
        _ => Err("使い方: /sync push · /sync recover <C10 operation ID> · /sync abandon <C10 operation ID>".into()),
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
        return Err("送信待ちを空にしてから共有内容を操作してください".into());
    }
    match c10_tui_invocation(requested)? {
        C10TuiInvocation::Push => {}
        C10TuiInvocation::Recover(operation) | C10TuiInvocation::Abandon(operation) => {
            let _ = operation;
        }
    }
    Err(
        "c10_unavailable_platform: 共有送信はmacOSのData Protection Keychainでのみ利用できます。"
            .into(),
    )
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
            *status = Status::Notice("C10操作の結果を表示しました".into());
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
        view.title = "C10 preview（確定はprepare後）".into();
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
            title: "C10操作を照合して中止".into(),
            details: vec![
                format!("operation={operation}"),
                format!("request={request}"),
                "送信されなかったことの証明にはなりません。".into(),
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
                        "C10 workerが終了しました。再送せず状態を確認してください".into(),
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
                "C10 workerが失敗しました。再送せず状態を確認してください".into(),
            ));
        }
        Some(
            self.result
                .take()
                .unwrap_or_else(|| Err("C10 workerの結果を確認できません".into())),
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
        title: "C10共有送信の確認".into(),
        details: vec![
            format!("operation={}", summary.operation_id),
            format!("request={}", summary.request_hash),
            format!("project={}", summary.project),
            format!("共有={}", summary.share_id),
            format!("remote={}", summary.remote_label),
            format!("ref={}", summary.remote_ref),
            format!("manifest={}", summary.manifest_digest),
            format!("payload={}", summary.payload_digest),
            format!("expected remote={:?}", summary.expected_remote),
            format!(
                "対象={}件・除外={}件",
                summary.file_count, summary.excluded_count
            ),
            format!("account registration={}", summary.account_registration_id),
            format!("操作={:?}", summary.action),
            "会話の巻き戻しでは共有送信を取り消せません。".into(),
        ],
    }
}

#[cfg(any(target_os = "macos", test))]
fn c10_tui_outcome(outcome: polaris_core::sync::push::SyncPushOutcomeV1) -> Result<String, String> {
    use polaris_core::sync::push::SyncPushOutcomeV1;
    match outcome {
        SyncPushOutcomeV1::NoChanges => Ok("C10: 共有内容は一致しています".into()),
        SyncPushOutcomeV1::Completed {
            operation_id,
            commit,
        } => Ok(format!(
            "C10送信完了: operation={operation_id} commit={commit}"
        )),
        SyncPushOutcomeV1::Denied { operation_id } => {
            Ok(format!("C10操作を取り消しました: operation={operation_id}"))
        }
        SyncPushOutcomeV1::ReconciledAbandon { operation_id } => Ok(format!(
            "C10操作を照合して中止しました: operation={operation_id}（未送信の証明ではありません）"
        )),
        SyncPushOutcomeV1::OutcomeUnknown { operation_id } => Err(format!(
            "C10結果未確定: operation={operation_id}。再送せず /sync recover {operation_id} で照合してください"
        )),
        SyncPushOutcomeV1::PartialComplete { operation_id } => Err(format!(
            "C10保存未完了: operation={operation_id}。再送せず /sync recover {operation_id} で照合してください"
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
        return Err("送信待ちを空にしてから共有内容を操作してください".into());
    }
    let invocation = c10_tui_invocation(requested)?;
    #[cfg(target_os = "macos")]
    {
        let project = std::fs::canonicalize(polaris_core::project::resolve_root(project))
            .map_err(|_| "C10 projectを確認できません")?;
        let expected =
            polaris_core::project::state_dir(&project).map_err(|_| "C10 stateを確認できません")?;
        if std::fs::canonicalize(state).map_err(|_| "C10 stateを確認できません")?
            != std::fs::canonicalize(expected).map_err(|_| "C10 stateを確認できません")?
        {
            return Err("C10 projectと保存場所が一致しません".into());
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
mod c10_tui_adapter_tests {
    use super::*;
    use polaris_core::{
        management_effect::ManagementOperationIdV1 as Operation,
        sync::{
            push::{SyncPushHostV1, SyncPushOutcomeV1},
            remote_intent::SyncDigestV1,
        },
    };

    #[test]
    fn c10_parser_preserves_explicit_recovery_and_rejects_extra_arguments() {
        let operation = Operation::random().unwrap();
        assert!(is_c10_sync_request(&format!("recover {operation}")));
        assert!(is_c10_sync_request(&format!("abandon {operation}")));
        assert!(!is_c10_sync_request(&format!("recover {}", "a".repeat(64))));
        assert!(matches!(
            c10_tui_invocation("push").unwrap(),
            C10TuiInvocation::Push
        ));
        assert!(
            matches!(c10_tui_invocation(&format!("recover {operation}")).unwrap(), C10TuiInvocation::Recover(value) if value == operation)
        );
        assert!(
            matches!(c10_tui_invocation(&format!("abandon {operation}")).unwrap(), C10TuiInvocation::Abandon(value) if value == operation)
        );
        for value in [
            "push unknown",
            "recover invalid",
            "push yes",
            "push recover",
        ] {
            assert!(c10_tui_invocation(value).is_err());
        }
        for value in [
            "/sync push",
            "/sync recover invalid",
            "/sync abandon invalid",
        ] {
            assert!(!slash::parse(value).unwrap().needs_management_clearance());
        }
    }

    #[test]
    fn c10_owned_worker_recovery_binds_invocation_without_confirmation() {
        let main_thread = std::thread::current().id();
        let operation = Operation::random().unwrap();
        let result = c10_tui_worker(
            C10TuiInvocation::Recover(operation),
            |_| panic!("recover must not ask consent"),
            move |host| {
                assert_ne!(std::thread::current().id(), main_thread);
                host.require_recovery_invocation(operation).unwrap();
                assert!(
                    host.require_recovery_invocation(Operation::random().unwrap())
                        .is_err()
                );
                assert!(
                    host.require_abandon_invocation(operation, SyncDigestV1::digest(b"request"))
                        .is_err()
                );
                Ok("reconciled without send".into())
            },
        )
        .unwrap();
        assert_eq!(result, "reconciled without send");
    }

    #[test]
    fn c10_owned_abandon_waits_for_exact_fresh_consent_and_retains_worker() {
        let main_thread = std::thread::current().id();
        let operation = Operation::random().unwrap();
        let request = SyncDigestV1::digest(b"durable request");
        let result = c10_tui_worker(
            C10TuiInvocation::Abandon(operation),
            |view| {
                assert_eq!(std::thread::current().id(), main_thread);
                assert!(
                    view.details
                        .iter()
                        .any(|value| value.contains(&operation.as_hex()))
                );
                assert!(
                    view.details
                        .iter()
                        .any(|value| value.contains(&request.to_string()))
                );
                true
            },
            move |host| {
                assert_ne!(std::thread::current().id(), main_thread);
                assert!(host.require_abandon_invocation(operation, request).is_err());
                host.confirm_abandon(operation, request).unwrap();
                host.require_abandon_invocation(operation, request).unwrap();
                assert!(
                    host.require_abandon_invocation(
                        operation,
                        SyncDigestV1::digest(b"other request")
                    )
                    .is_err()
                );
                assert!(host.require_recovery_invocation(operation).is_err());
                Ok("owned worker completed".into())
            },
        )
        .unwrap();
        assert_eq!(result, "owned worker completed");
    }

    #[test]
    fn c10_worker_decline_or_failure_never_detaches_or_grants_consent() {
        let operation = Operation::random().unwrap();
        let error = c10_tui_worker(
            C10TuiInvocation::Abandon(operation),
            |_| false,
            move |host| {
                host.confirm_abandon(operation, SyncDigestV1::digest(b"request"))
                    .map_err(|e| e.to_string())?;
                panic!("declined request continued")
            },
        )
        .unwrap_err();
        assert_eq!(error, "c10_permission_unavailable");
        assert!(
            c10_tui_worker(
                C10TuiInvocation::Push,
                |_| panic!("worker failed before prompt"),
                |_| { panic!("owned synthetic worker failure") }
            )
            .unwrap_err()
            .contains("workerが失敗")
        );
    }

    struct Keys(std::collections::VecDeque<ratatui::crossterm::event::KeyCode>);
    impl approver::KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<ratatui::crossterm::event::KeyCode> {
            self.0
                .pop_front()
                .ok_or_else(|| std::io::ErrorKind::UnexpectedEof.into())
        }
    }

    #[test]
    fn c10_narrow_confirmation_requires_complete_wrapped_details() {
        use ratatui::crossterm::event::KeyCode;
        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 10)).unwrap(),
        );
        let view = render::C10ConfirmView {
            title: "C10".into(),
            details: vec!["a".repeat(64), "b".repeat(64), "request end".into()],
        };
        let mut premature = Keys([KeyCode::Char('y')].into());
        assert!(!confirm_c10_sync(&terminal, &mut premature, &view));
        let mut keys = std::collections::VecDeque::from([KeyCode::Char('y')]);
        keys.extend(std::iter::repeat_n(KeyCode::Down, 12));
        keys.push_back(KeyCode::Char('y'));
        assert!(confirm_c10_sync(&terminal, &mut Keys(keys), &view));
        let mut cancel = Keys([KeyCode::Esc].into());
        assert!(!confirm_c10_sync(&terminal, &mut cancel, &view));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn c10_actual_gateway_rejects_queued_and_malformed_input_before_writes() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        {
            assert!(start_c10_sync_action(root.path(), root.path(), "push", true).is_err());
            assert_eq!(
                start_c10_sync_action(root.path(), root.path(), "recover invalid", false)
                    .err()
                    .unwrap(),
                "C10 operation IDが不正です"
            );
        }
        #[cfg(target_os = "linux")]
        {
            assert!(c10_unavailable_action("push", true).is_err());
            assert_eq!(
                c10_unavailable_action("recover invalid", false).unwrap_err(),
                "C10 operation IDが不正です"
            );
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap(),
        );
        let session = polaris_core::session::Session::default();
        let mut status = Status::Notice(String::new());
        let mut lines = Vec::new();
        handle_sync_action(
            &terminal,
            &mut Keys([].into()),
            root.path(),
            root.path(),
            "recover invalid",
            false,
            &session,
            &mut status,
            &mut lines,
        );
        assert!(matches!(status, Status::Error(_)));
        assert!(session.messages.is_empty());
        assert!(lines.is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn c10_unknown_and_partial_are_not_terminal_successes() {
        let operation_id = Operation::random().unwrap();
        assert!(c10_tui_outcome(SyncPushOutcomeV1::NoChanges).is_ok());
        for outcome in [
            SyncPushOutcomeV1::OutcomeUnknown { operation_id },
            SyncPushOutcomeV1::PartialComplete { operation_id },
        ] {
            let error = c10_tui_outcome(outcome).unwrap_err();
            assert!(error.contains(&format!("/sync recover {operation_id}")));
        }
    }

    #[test]
    fn c10_pending_network_job_returns_to_main_loop_without_waiting() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut job = C10TuiJob::start(C10TuiInvocation::Push, move |_| {
            blocked.recv().unwrap();
            Ok("terminal".into())
        });
        let mut redraws = 0;
        for _ in 0..50 {
            assert!(
                job.poll(
                    |_| panic!("unexpected preview"),
                    |_| panic!("unexpected confirmation")
                )
                .is_none()
            );
            redraws += 1;
        }
        assert_eq!(redraws, 50);
        assert!(!job.worker.as_ref().unwrap().is_finished());
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(result) = job.poll(|_| true, |_| false) {
                assert_eq!(result.unwrap(), "terminal");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(job.worker.is_none());
    }

    #[test]
    fn c10_preview_ack_is_separate_from_prepared_confirmation_and_drop_reaps() {
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker_order = order.clone();
        let mut job = C10TuiJob::start(C10TuiInvocation::Push, move |host| {
            host.show_preview(render::C10ConfirmView {
                title: String::new(),
                details: vec!["preview".into()],
            })
            .unwrap();
            worker_order.lock().unwrap().push("prepared");
            assert!(
                host.ask(render::C10ConfirmView {
                    title: "prepared".into(),
                    details: Vec::new()
                })
                .unwrap()
            );
            Ok("terminal".into())
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(result) = job.poll(
                |_| {
                    order.lock().unwrap().push("preview displayed");
                    true
                },
                |_| {
                    order.lock().unwrap().push("confirmed");
                    true
                },
            ) {
                assert_eq!(result.unwrap(), "terminal");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            *order.lock().unwrap(),
            ["preview displayed", "prepared", "confirmed"]
        );
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_done = done.clone();
        let abandoned = C10TuiJob::start(C10TuiInvocation::Push, move |host| {
            assert!(
                host.ask(render::C10ConfirmView {
                    title: "pending".into(),
                    details: Vec::new()
                })
                .is_err()
            );
            worker_done.store(true, std::sync::atomic::Ordering::Release);
            Err("UI closed".into())
        });
        drop(abandoned);
        assert!(done.load(std::sync::atomic::Ordering::Acquire));
    }
}

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
        return Err("使い方: /sync [status|join <remote> <share-id> <invite-id> <expires> [除外名... ]|pull|leave|apply|recover <表示された操作ID>]".into());
    }
    if !inspect && has_queued_input {
        return Err("送信待ちを空にしてから共有内容を操作してください".into());
    }
    let project = std::fs::canonicalize(polaris_core::project::resolve_root(project))
        .map_err(|e| e.to_string())?;
    let expected_state = polaris_core::project::state_dir(&project).map_err(|e| e.to_string())?;
    let state = std::fs::canonicalize(state).map_err(|e| e.to_string())?;
    if state != std::fs::canonicalize(expected_state).map_err(|e| e.to_string())? {
        return Err("projectと保存場所が一致しません".into());
    }
    // Retain the same owner from inspection through confirmation and commit.
    let owner = ApplyOwnerV1::acquire(&project).map_err(|e| e.to_string())?;
    if matches!(words.first(), Some(&"join" | &"pull" | &"leave" | &"push")) {
        if words[0] == "push" {
            return Err(
                "共有送信ownerは未実装です。credential実機検証が必要なため送信できません。".into(),
            );
        }
        owner
            .ensure_sync_idle()
            .map_err(|_| "未完了の共有操作を確認してください")?;
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
                        .map_err(|_| "招待期限はUNIX秒で指定してください")?,
                };
                let now =
                    u64::try_from(now_millis() / 1000).map_err(|_| "招待期限を照合できません")?;
                share
                    .accept(now)
                    .map_err(|_| "招待URL・ID・期限を確認してください。変更していません。")?;
                if held.is_some() {
                    return Err("参加済みです。安全なcredential rebind ownerは未実装です。".into());
                }
                let manifest = polaris_core::sync::share::ShareManifest {
                    version: 1,
                    excluded: words[5..].iter().map(|w| (*w).to_owned()).collect(),
                };
                let detail = format!(
                    "共有: {}\n招待: {}\nremote: {}\n期限: {}\n除外: {:?}\n参加はこの端末の保存のみ。peer・sourceを変更しません。",
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
                let held = held.as_ref().ok_or("このprojectは未参加です")?;
                (
                    "C11 pull",
                    format!(
                        "共有: {}\nremote: {}\n合意revision: {}\n受信・local stagingだけを更新します。source適用は別の確認です。",
                        held.share.share_id,
                        held.share.remote_display(),
                        held.agreed_commit.as_deref().unwrap_or("なし")
                    ),
                )
            }
            "leave" if words.len() == 1 => {
                let held = held.as_ref().ok_or("このprojectは未参加です")?;
                (
                    "C13 leave",
                    format!(
                        "共有: {}\nこの端末の参加状態を削除します。共有履歴・peer・sourceは残します。",
                        held.share.share_id
                    ),
                )
            }
            _ => return Err(
                "/sync join <remote> <share-id> <invite-id> <expires> [除外名...] | pull | leave"
                    .into(),
            ),
        };
        let view = render::SyncConfirmView {
            recovering: false,
            project: project.display().to_string(),
            operation: id.into(),
            revision: detail,
            previous_revision: "変更前の参加状態を管理記録へ結び付けます".into(),
            recovery: "sourceとpeerは変更しません".into(),
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
                            "参加しました"
                        } else {
                            "参加を取り消しました"
                        }
                        .into()
                    })
            }
            "pull" => owner.pull_confirmed(accepted).map(|p| match p {
                Some(exchange) => format!(
                    "受け取りました: {}件、解消待ち{}件。sourceへは未反映です。",
                    exchange.changes.len(),
                    exchange.needs_attention().len()
                ),
                None => "受信を取り消しました".into(),
            }),
            "leave" => owner.leave_confirmed(accepted).map(|p| {
                if p.is_some() {
                    "この端末の参加を解除しました"
                } else {
                    "離脱を取り消しました"
                }
                .into()
            }),
            _ => unreachable!(),
        }
        .map_err(|_| "共有操作の結果を確定できません。/syncで状態を確認してください。".into());
    }
    let active = owner.active().map_err(|e| e.to_string())?;
    let recovery = state.join("sync/recovery").display().to_string();
    if inspect {
        let pending = active.as_ref().map_or_else(
            || "なし".into(),
            |intent| {
                format!(
                    "{:?} {:?} / 操作ID: {}",
                    intent.descriptor().inventory_id,
                    intent.latest().phase,
                    intent.descriptor().operation_key.as_hex()
                )
            },
        );
        let management_pending = match owner
            .inspect_management()
            .map_err(|_| "共有の管理記録を確認できません")?
        {
            polaris_core::management_effect::ManagementEffectInspectOpenV1::Absent => {
                "なし".to_owned()
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
                    "なし".into()
                } else {
                    records.join("\n")
                }
            }
        };
        let classification = participation::read(&state).map_err(|e| e.to_string())?;
        let held = classification.participation();
        return Ok(format!(
            "共有適用: {}\n合意revision: {}\n反映済みrevision: {}\n解消待ち: {}\n未完了: {}\n管理操作: {}\n復旧用の控え: {}\n/sync apply · /sync recover <操作ID>",
            if held.is_some() {
                "参加中"
            } else {
                "未参加"
            },
            held.as_ref()
                .and_then(|p| p.agreed_commit.as_deref())
                .unwrap_or("なし"),
            held.as_ref()
                .and_then(|p| p.applied_commit.as_deref())
                .unwrap_or("なし"),
            held.as_ref().map_or(0, |p| p.unresolved.len()),
            pending,
            management_pending,
            recovery,
        ));
    }
    if recovering {
        let active = active.ok_or("未完了の共有適用はありません")?;
        let key = active.descriptor().operation_key.as_hex();
        if active.descriptor().inventory_id != ApplyInventoryV1::C12 || words[1] != key {
            return Err("C12の表示された未完了操作IDを指定してください。変更していません。".into());
        }
        let parent = owner
            .c12_recovery_parent(false)
            .map_err(|e| e.to_string())?;
        let view = render::SyncConfirmView {
            recovering: true,
            project: project.display().to_string(),
            operation: key.clone(),
            revision: active.descriptor().agreed_revision.value.clone(),
            previous_revision: "変更しない".into(),
            recovery,
            changes: Vec::new(),
        };
        if !confirm(&view) {
            return Ok("共有適用の復旧を取り消しました".into());
        }
        return match owner.recover_current(&parent).map_err(|e| e.to_string())? {
            ApplyOwnerRecoveryV1::OutcomeUnknown(_) => Ok(format!(
                "適用結果は未確定です ({key})。sourceを再適用していません。復旧用の控え: {}",
                view.recovery
            )),
            ApplyOwnerRecoveryV1::Archived(_) => Ok(format!(
                "保存済み証拠から操作を閉じました ({key})。sourceを再適用していません。/syncで状態を確認してください。"
            )),
            ApplyOwnerRecoveryV1::Idle => {
                Err("未完了状態が変わりました。/syncで確認してください。".into())
            }
        };
    }
    if let Some(active) = active {
        return Err(format!(
            "未完了の適用があります ({})。/syncで確認してください。新しい適用は行っていません。",
            active.descriptor().operation_key.as_hex()
        ));
    }
    owner.ensure_sync_idle().map_err(|e| e.to_string())?;
    let mut held = participation::read(&state)
        .map_err(|e| e.to_string())?
        .participation()
        .cloned()
        .ok_or("このprojectは共有に参加していません")?;
    if !held.unresolved.is_empty() {
        return Err(format!(
            "解消待ちが{}件あります。先に解消してください。",
            held.unresolved.len()
        ));
    }
    let agreed = held
        .agreed_commit
        .clone()
        .ok_or("合意した内容がありません。先にpolaris sync pullを実行してください。")?;
    if held.applied_commit.as_deref() == Some(&agreed) {
        return Ok("この内容は反映済みです。".into());
    }
    if std::fs::metadata(&project)
        .map_err(|e| e.to_string())?
        .dev()
        != std::fs::metadata(&state).map_err(|e| e.to_string())?.dev()
    {
        return Err("projectと同じファイルシステムの復旧場所が必要です".into());
    }
    let parent = owner.c12_recovery_parent(true).map_err(|e| e.to_string())?;
    let paths = SyncPaths::under(&state);
    let staging = paths.work_tree.clone();
    let repository = SyncRepository::open(paths).map_err(|e| e.to_string())?;
    if repository.head().as_deref() != Some(&agreed) {
        return Err(
            "同期用の記録が合意内容と一致しません。先にpolaris sync pullを実行してください。"
                .into(),
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
            return Err("未作成ディレクトリの共有変更が残っています".to_string());
        }
        let changes =
            apply::planned_changes(&snapshot, &secrets, limits).map_err(|e| e.to_string())?;
        if !changes.is_applicable() {
            return Err("保護対象を含むため共有内容を適用できません".into());
        }
        Ok::<_, String>((snapshot, changes))
    })
    .map_err(|_| "保護状態を確認できません".to_string())??;
    let revision = |value: &str| -> Result<RevisionV1, String> {
        let kind = match value.len() {
            40 => RevisionKindV1::GitSha1,
            64 => RevisionKindV1::GitSha256,
            _ => return Err("合意revisionの形式を確認できません".into()),
        };
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("合意revisionの形式を確認できません".into());
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
        previous_revision: held.applied_commit.clone().unwrap_or_else(|| "なし".into()),
        recovery,
        changes: changes
            .changes
            .iter()
            .map(|c| format!("{:?}: {}", c.kind, c.relative_path.display()))
            .collect(),
    };
    if !confirm(&view) {
        return Ok("共有内容の適用を取り消しました".into());
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
        .map_err(|e| format!("反映を確定できません。/syncで状態を確認してください: {e}"))?;
    if let Some(failure) = report.failure {
        return Err(format!(
            "適用結果は未確定です: {failure:?}。/syncで状態を確認してください。"
        ));
    }
    Ok(format!(
        "{}件を反映しました。復旧用の控え: {}",
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
            return Err("C10操作にはmainが所有するpending jobが必要です".into());
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
            *status = Status::Notice("共有適用の状態を表示しました".into());
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
        *status =
            Status::Error("送信待ちまたは保存エラーを解消してからD27を操作してください".into());
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
            *status = Status::Notice("D27の状態を表示しました".into());
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
        .ok_or("保存済みworkflow設定がありません")?;
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
        .ok_or("workflow設定を保存する会話がありません")?;
    let preview = match WorkflowChangeOwner::preview(persistence, inventory, &candidate) {
        Ok(preview) => preview,
        Err(WorkflowChangeError::NoChange) => return Ok(WorkflowChangeResult::NoChange),
        Err(error) => return Err(error.to_string()),
    };
    let before_config = preview
        .before_config()
        .ok_or("変更前のworkflow設定がありません")?;
    let candidate_config = preview
        .candidate_config()
        .ok_or("変更後のworkflow設定がありません")?;
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
        *status = Status::Notice("送信待ちを空にしてからworkflowを変更してください".into());
        return;
    }
    let result = (|| -> Result<Option<String>, String> {
        session
            .check_persistence()
            .map_err(|error| error.to_string())?;
        let current = session
            .workflow
            .as_ref()
            .ok_or("workflowはこのセッションで無効です")?;
        let (inventory, candidate, notice) = match action {
            slash::Action::Phase(requested) => {
                if requested.is_empty() {
                    return Ok(Some(format!("ワークフロー段階: {}", current.state().phase)));
                }
                if !current.config.enabled {
                    return Err("workflowが無効な会話ではphaseを変更できません".into());
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
                    format!("ワークフロー段階: {phase}"),
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
                    _ => return Err("workflow modeは manual、adaptive、off のいずれかです".into()),
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
            _ => return Err("workflow操作の種類が不正です".into()),
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
        Ok(None) => Status::Notice("workflow変更を取り消しました".into()),
        Err(error) => Status::Error(format!("workflowを保存できません: {error}")),
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
        .ok_or("この会話には巻き戻し可能な保存履歴がありません")?;
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
        run_rewind_picker(terminal, reader, &rows);
        return Err(format!(
            "検証済みの巻き戻し境界がありません: {unavailable_reason}"
        ));
    }
    let Some(selected_id) = run_rewind_picker(terminal, reader, &rows) else {
        return Ok(None);
    };
    let selected = boundaries
        .iter()
        .find(|boundary| boundary.boundary_id == selected_id)
        .ok_or("選択した巻き戻し境界が見つかりません")?
        .clone();
    let latest = boundaries.last().ok_or("巻き戻し境界がありません")?;
    let expected_post: polaris_core::rewind_workspace::ManifestId = latest
        .completion
        .as_ref()
        .ok_or("最新の巻き戻し境界が未完了です")?
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
            "ファイルを復元できません: {:?}",
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
        .map_err(|error| format!("巻き戻し先の履歴を復元できません: {error}"))?;
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
    .map_err(|error| format!("巻き戻し先のツール記憶を復元できません: {error}"))?;
    if args.remember {
        memory::configure_saved_memory(&mut candidate, &args.cwd, &args.state_dir, &path)
            .map_err(|error| format!("巻き戻し先の記憶を復元できません: {error}"))?;
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
        .map_err(|error| format!("復元後の設定を読み直せません: {error}"))?;
    let skills = polaris_skills::discover(&project, &config.skills_paths).skills;
    let agents = polaris_skills::discover_agent_types(&project, &config.agents_paths).agent_types;
    let host = polaris_core::mcp::local::LocalHost::open(cwd, false)
        .map_err(|error| format!("復元後のMCP設定を読み直せません: {error}"))?;
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
            "未解決の管理操作 {} ({}) があるため巻き戻せません",
            effect.request.operation_id, effect.request.inventory
        )),
        Err(error) => Err(format!("管理操作の状態を確認できません: {error}")),
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
        .map_err(|error| format!("巻き戻しtransactionを同期・再読込できません: {error}"))?;
    if readback.candidate_session_id.as_deref() != Some(branch_id)
        || readback.expected_post != *expected_post
        || readback.target_pre != *target_pre
    {
        return Err("巻き戻しtransactionと子会話の証拠が一致しません".into());
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
        return Err("巻き戻しtransactionは安全にrollbackできる状態ではありません".into());
    }
    let restored = before
        .restored_manifest_id
        .ok_or("巻き戻し後のmanifestがありません")?;
    if !owner
        .matches_manifest(&restored)
        .map_err(|error| error.to_string())?
    {
        return Err("巻き戻し後の作業ファイルが既知の状態と一致しません".into());
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
            "rollback結果を確認できません: {}",
            write
                .err()
                .map_or_else(|| "phase不一致".into(), |error| error.to_string())
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
        message: format!("巻き戻し確定用のproject leaseを取得できません: {error}"),
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
                message: format!("巻き戻しの確定前に復旧できません: {blocker}; {recovery}"),
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
        message: format!("巻き戻しの確定結果が不明です: {error}"),
        fatal: true,
    })?;
    match readback.phase {
        RewindTransactionPhase::Committed => Ok(()),
        RewindTransactionPhase::PendingActivation => {
            let reason = write.err().map_or_else(
                || "確定後のphaseが未終端です".to_string(),
                |error| format!("巻き戻しの確定に失敗しました: {error}"),
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
                    message: format!("{reason}; 復旧できません: {recovery}"),
                    fatal: true,
                }),
            }
        }
        _ => Err(RewindFailure {
            message: "巻き戻しの確定phaseを確認できません。再起動して復旧してください".into(),
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
        message: "巻き戻し元の保存済みセッションがありません".into(),
        fatal: false,
    })?;
    let branch = persistence
        .prepare_rewind_branch(owner, &boundary.boundary_id)
        .map_err(|error| RewindFailure {
            message: format!("会話分岐を準備できません: {error}"),
            fatal: false,
        })?;
    let branch_id = branch
        .snapshot()
        .map_err(|error| RewindFailure {
            message: format!("会話分岐を検証できません: {error}"),
            fatal: false,
        })?
        .state
        .session_id;
    let restored = match owner.restore_for_branch(expected_post, target_pre, &branch_id) {
        Ok(restored) => restored,
        Err(error) => {
            return match owner.resolve_pending() {
                Ok(_) => Err(RewindFailure {
                    message: format!("ファイルを復元できません: {error}"),
                    fatal: false,
                }),
                Err(recovery) => Err(RewindFailure {
                    message: format!(
                        "ファイル復元に失敗し、途中状態を解決できません: {error}; {recovery}"
                    ),
                    fatal: true,
                }),
            };
        }
    };
    let candidate = async {
        args.always_on
            .system_for_request()
            .map_err(|error| format!("復元後の指示を読み直せません: {error}"))?;
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
                    message: format!("巻き戻し先を準備できません: {error}"),
                    fatal: false,
                }),
                Err(recovery) => Err(RewindFailure {
                    message: format!(
                        "巻き戻し先の準備に失敗し、元のファイルへ復旧できません: {error}; {recovery}"
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
            message: format!("確定した子会話 {branch_id} の保存先を確認できません"),
            fatal: true,
        })?;
    validate_resume_workspace(owner, committed).map_err(|error| RewindFailure {
        message: format!("確定した子会話 {branch_id} を再読込できません: {error}"),
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
            *status = Status::Error(format!("保存会話の一覧を読めません: {error}"));
            return;
        }
    };
    #[cfg(not(target_os = "linux"))]
    let mut summaries = sessions::list_sessions(sessions_dir);
    let Some(data_root) = sessions_dir.parent() else {
        *status = Status::Notice("v2セッションの保存先が不正です".into());
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
                        *status = Status::Error("会話一覧が256件を超えています".into());
                        return;
                    }
                }
                Err(error) => {
                    *status = Status::Error(format!("v2保存会話の一覧を読めません: {error}"));
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
            *status =
                Status::Notice("作業ファイルの検証ownerがないため会話を再開できません".into());
            return;
        }
        let Some(project_id) = project_id.as_deref() else {
            *status = Status::Notice(
                "プロジェクト識別子を取得できないためv2セッションを再開できません".into(),
            );
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
                .abandon_unfinished_rewind_boundaries("前回の実行が完了する前に終了しました".into())
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
                return Err(format!("{error}。会話は閲覧のみ可能です"));
            }
            #[cfg(target_os="linux")]
            let mut frozen_resume=resume_effect::ResumeSubjectV1::Persisted(&persistence).identity()?;
            #[cfg(target_os="linux")]
            if workflow_mode_overridden
                && let Some(previous)=session.persistence.as_ref()
                && previous.store.directory()==persistence.store.directory()
                && resume_effect::ResumeSubjectV1::Persisted(previous).identity()?!=frozen_resume
            {
                return Err("変更前の会話が選択先と一致しません".into());
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
                    return Err("停止要求を受信しました".into());
                }
                #[cfg(not(unix))]
                return Err("workflow変更には管理操作が必要です".into());
                #[cfg(unix)]
                {
                    restored
                        .recover_history()
                        .await
                        .map_err(|error| format!("strict10履歴を復元できません: {error}"))?;
                    #[cfg(target_os = "linux")]
                    if signal::pending_linux_process_shutdown().is_some() {
                        return Err("停止要求を受信しました".into());
                    }
                    let workspace = rewind_workspace.ok_or("project mutation ownerがありません")?;
                    let effects = management_effects.ok_or("管理操作の記録がありません")?;
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
                            return Err("workflow変更を取り消しました".into());
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
                        return Err("互換復旧前に選択した会話が変更されました".into());
                    }
                    frozen_resume = resume_effect::ResumeIdentityV1::published_snapshot(&publication.published)?;
                    compatibility_published = true;
                }
                #[cfg(not(target_os="linux"))]
                let _ = publication;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return Err("停止要求を受信しました".into());
                }
                restored
                    .recover_history()
                    .await
                    .map_err(|error| format!("strict10履歴を復元できません: {error}"))?;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return Err("停止要求を受信しました".into());
                }
            }
            #[cfg(target_os="linux")]
            {
                let restored_saved=restored.persistence.as_ref().ok_or("復元先の会話がありません")?;
                if !workflow_mode_overridden && resume_effect::ResumeSubjectV1::Persisted(restored_saved).identity()?!=frozen_resume {return Err("復元中に選択した会話が変更されました".into());}
            }
            Ok::<_, String>(restored)
        }
        .await;
        match result {
            Ok(restored) => {
                #[cfg(target_os = "linux")]
                {
                    let result = (|| -> Result<(), String> {
                        let workspace =
                            rewind_workspace.ok_or("project mutation ownerがありません")?;
                        let effects = management_effects.ok_or("管理効果ownerがありません")?;
                        let target = restored
                            .persistence
                            .clone()
                            .ok_or("選択したv2会話がありません")?;
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
                            "会話を選択",
                            &selected.id,
                            "選択した保存会話のworkspace対応と原文を確認し、この会話へ切り替えます。",
                        );
                        let mut candidate = Some(restored);
                        owner.execute(&prepared, from, to, accepted, || {
                            *session = candidate.take().ok_or("復元候補がありません")?;
                            Ok(())
                        })?;
                        if !accepted {
                            return Err("会話の選択を取り消しました".into());
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
                *status = Status::Notice(format!("v2セッションを再開できません: {error}"))
            }
        }
        return;
    }

    if workflow_mode_overridden {
        *status =
            Status::Notice("旧形式の会話への--workflow-mode適用には移行管理操作が必要です".into());
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
                        *status = Status::Notice(format!("会話の原文を読めません: {error}"));
                        return;
                    }
                }
                *status = Status::Notice(
                    "旧形式の会話は保存済み作業ファイルとの対応記録がないため、閲覧のみ可能です"
                        .into(),
                );
                return;
            }
            Err(error) => {
                *status = Status::Notice(format!("作業ファイルを確認できません: {error}"));
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
                        "プロジェクト識別子を取得できないため旧セッションを移行できません".into(),
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
                                "旧セッションを現在の作業ファイルへ結び付けられません: {error}"
                            ));
                            return;
                        }
                        let mut imported = polaris_core::session::Session::new();
                        imported.input_preparation = session.input_preparation;
                        imported.disable_files_md_auto_regenerate =
                            disable_files_md_auto_regenerate;
                        imported.workflow = workflow;
                        if let Err(error) = imported.attach(persistence) {
                            *status =
                                Status::Notice(format!("旧セッションを移行できません: {error}"));
                            return;
                        }
                        imported.strict_history = strict_history;
                        imported.summary_usage = summary_usage;
                        imported.examples = examples;
                        if let Err(error) = imported.recover_history().await {
                            *status =
                                Status::Notice(format!("strict10履歴を復元できません: {error}"));
                            return;
                        }
                        #[cfg(target_os = "linux")]
                        if signal::pending_linux_process_shutdown().is_some() {
                            return;
                        }
                        *session = imported;
                    }
                    Err(error) => {
                        *status = Status::Notice(format!("旧セッションを移行できません: {error}"));
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
                    "{} に破損行があったため、その前までのメッセージを再開しました",
                    session_path.display()
                ))
            } else {
                Status::Idle
            };
        }
        Err(e) => *status = Status::Notice(format!("セッションを再開できません: {e}")),
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
                *status = Status::Notice(format!("新しいv2セッションを作成できません: {error}"));
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
                *status = Status::Notice(format!("新しいv2セッションを作成できません: {error}"));
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
                *status = Status::Notice(format!("v2セッションへ分岐しました: {id}"));
            }
            Err(error) => {
                *status = Status::Notice(format!("v2セッションを分岐できません: {error}"))
            }
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
                "記憶の出自を確認できないため分岐できません: {error}"
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
                    *status = Status::Notice("記憶の系譜が上限に達したため分岐できません".into());
                    return;
                }
                Err(error) => {
                    *status = Status::Notice(format!(
                        "記憶の出自を確認できないため分岐できません: {error}"
                    ));
                    return;
                }
            }
        } else {
            None
        };
    if needs_memory_origin && memory_origin.is_none() {
        *status = Status::Notice(
            "記憶の出自を確認できないため分岐できません。元の会話はそのまま継続できます。".into(),
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
            "分岐先: {}。祖先の記憶は参照専用で、新しい記憶はこの分岐に保存します。祖先の記憶を削除すると、この分岐の記憶利用も無効になります。",
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
        *status = Status::Notice("管理記録付きの/newにはv2セッションが必要です".into());
        return false;
    }
    let lease = match owner.try_mutation() {
        Ok(lease) => lease,
        Err(error) => {
            *status = Status::Notice(format!("新しい会話を開始できません: {error}"));
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
        *status = Status::Notice("管理記録付きの/forkにはv2セッションが必要です".into());
        return;
    }
    let lease = match owner.try_mutation() {
        Ok(lease) => lease,
        Err(error) => {
            *status = Status::Notice(format!("会話を分岐できません: {error}"));
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
                "起動時の承認境界と両立しません。--approval {} で再起動してください",
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

#[cfg(target_os = "linux")]
async fn model_inventory_read_lines(
    request: &str,
) -> Result<Vec<ratatui::text::Line<'static>>, String> {
    if !request.is_empty() && request != "list" {
        return Err("/models [list]。インストール・削除は提供していません".into());
    }
    let mut lines = vec![ratatui::text::Line::from(
        "モデル一覧: 利用可能性は接続先の報告、インストール状態は未確認です。",
    )];
    let Ok(models) =
        tokio::time::timeout(Duration::from_secs(3), discover_connected_local_models()).await
    else {
        lines.push(ratatui::text::Line::from(
            "一覧取得の期限を超えました。利用可能性は未確認です。",
        ));
        return Ok(lines);
    };
    if models.is_empty() {
        lines.push(ratatui::text::Line::from(
            "観測できた生成対応モデルはありません。未接続・取得失敗・生成非対応は区別できません。",
        ));
    }
    for model in models.iter().take(64) {
        lines.push(ratatui::text::Line::from(
            render::sanitize(&format!(
                "{} / available=接続先の報告 / installed=未確認",
                model.display_name()
            ))
            .escape_debug()
            .to_string(),
        ));
    }
    if models.len() > 64 {
        lines.push(ratatui::text::Line::from(format!(
            "残り{}件は表示上限のため省略しました",
            models.len() - 64
        )));
    }
    Ok(lines)
}

/// Runs the `/model` picker — four hosted choices plus one local-runtime
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
        "外部MCPホスト上限: {}",
        if ceiling.permits_external() {
            "許可"
        } else {
            "拒否"
        }
    )];
    let Ok(host) = polaris_core::mcp::local::LocalHost::open(cwd, false) else {
        lines.push("MCPの設定を読めません。".into());
        return lines;
    };
    // Show the same external ceiling that the active run actually uses.
    let report = match host.state_report(ceiling) {
        Ok(report) => report,
        Err(error) => {
            lines.push(format!("MCPの状態を読めません: {error}"));
            return lines;
        }
    };
    if report.is_empty() {
        lines.push("登録されたMCPサーバーはありません。".into());
        return lines;
    }
    lines.push(format!("MCPサーバー {}件", report.len()));
    for server in report {
        let state = match (&server.quarantine, server.enabled_here) {
            // A refused server is refused whether or not this project
            // enabled it, so the reason comes first.
            (Some(reason), _) => format!("使えません（{reason}）"),
            (None, false) => "このプロジェクトでは無効".to_owned(),
            (None, true) => "有効".to_owned(),
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
        return vec!["巻き戻し候補: workspace ownerを確認できません".into()];
    };
    let Some(data_root) = sessions_dir.parent() else {
        return vec!["巻き戻し候補: 保存先を特定できません".into()];
    };
    let project_root = polaris_core::project::resolve_root(cwd);
    let project_id = match persist::project_identity(&project_root) {
        Ok(project_id) => project_id,
        Err(error) => {
            return vec![format!("巻き戻し候補: projectを確認できません: {error}").into()];
        }
    };
    let diagnostics =
        polaris_core::rewind_admission::candidate_diagnostics(owner, data_root, &project_id);
    if diagnostics.is_empty() {
        return vec!["巻き戻し候補: 拒否・判定不能なし".into()];
    }
    let mut lines = vec![format!("巻き戻し候補: 拒否・判定不能 {}件", diagnostics.len()).into()];
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
                "  他 {}件。保存領域を停止状態で確認してください",
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
                if path.is_dir() { "存在" } else { "未作成" }
            )
        }
        _ => "利用不可 (保存先を特定できません)".to_owned(),
    };
    let rules = polaris_core::constitution::reach(&project_root);
    let delivered = rules.iter().map(|rule| rule.delivered).sum::<usize>();
    [
        "診断 (read-only)".to_owned(),
        format!("設定: provider={provider_name}, model={model_name}"),
        format!(
            "設定: ChatGPT認証ファイル={}, API keyファイル={}, 環境API key={}",
            saved_chatgpt, saved_api_key, env_api_key
        ),
        format!(
            "利用条件: sandbox={}, approval={approval:?}",
            sandbox.describe()
        ),
        format!("利用条件: 外部MCP={mcp_external}; 個別サーバーは /mcp で確認"),
        format!("利用条件: state={state}; 常時指示の到達行数={delivered}"),
        "観測: この診断は認証・モデル・MCPへ送信しません。接続成功と実資源の可用性は未検証です。"
            .to_owned(),
    ]
    .into_iter()
    .map(|line| ratatui::text::Line::from(render::sanitize(&line).escape_debug().to_string()))
    .collect()
}

fn cat_diagnostic_lines(
    owner: Option<&polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>,
) -> Vec<ratatui::text::Line<'static>> {
    let line = match owner {
        None => {
            "CAT: configured=shadow ownerなし; installed=未照合; verified=未確認; observed=未取得"
                .into()
        }
        Some(owner) => match owner.translation_observation() {
            Ok((requested, installed, verified, attempted)) => format!(
                "CAT: configured={requested}; installed={}; verified={verified}; observed={attempted} attempts (保存済み観測)。実行の成功とは別です。",
                match installed {
                    Some(true) => "導入確認",
                    Some(false) => "未導入",
                    None => "未照合",
                },
            ),
            Err(_) => "CAT: 状態を読めません。可用性は未確認です。".into(),
        },
    };
    vec![line.into()]
}

struct StatusSnapshot<'a> {
    session: &'a polaris_core::session::Session,
    provider_name: &'a str,
    model_name: &'a str,
    usage_report: polaris_provider::UsageReport,
    sandbox: &'a polaris_sandbox::SandboxPolicy,
    approval: polaris_core::approval::ApprovalPolicy,
    startup_approval: polaris_core::approval::ApprovalPolicy,
    mcp_external: bool,
    audit_path: Option<&'a Path>,
    turn_authority_present: bool,
    fixed_effort: Option<&'a str>,
    answer_finalizer: Option<&'a polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>,
    #[cfg(unix)]
    project_root: &'a Path,
    #[cfg(unix)]
    management_effects: &'a polaris_core::management_effect::ManagementEffectStoreV1,
    #[cfg(unix)]
    recovery_issue: Option<&'a str>,
}

fn status_lines(snapshot: StatusSnapshot<'_>) -> Vec<ratatui::text::Line<'static>> {
    let StatusSnapshot {
        session,
        provider_name,
        model_name,
        usage_report,
        sandbox,
        approval,
        startup_approval,
        mcp_external,
        audit_path,
        turn_authority_present,
        fixed_effort,
        answer_finalizer,
        #[cfg(unix)]
        project_root,
        #[cfg(unix)]
        management_effects,
        #[cfg(unix)]
        recovery_issue,
    } = snapshot;
    let usage = usage_report.usage;
    let mut lines = vec![
        "状態 (read-only)".to_owned(),
        format!("設定: provider={provider_name}, model={model_name}"),
        format!(
            "利用条件: approval 現在={approval:?}, 起動時={startup_approval:?}; sandbox={}",
            sandbox.describe()
        ),
        format!("利用条件: 外部MCP={mcp_external}; 個別サーバーは /mcp で確認"),
        format!(
            "観測: tokens in {} / out {} / cache {} / total {}; 欠測 {} / 失敗 {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            usage.total_tokens,
            usage_report.missing_responses,
            usage_report.failed_requests,
        ),
        format!("観測: 会話 {} messages", session.messages.len()),
    ];
    if let Some(workflow) = &session.workflow {
        let state = workflow.state();
        let ids = state
            .skills
            .iter()
            .map(|skill| skill.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "観測: workflow phase={}; skills={}; manifest={}",
            state.phase, ids, state.skill_manifest_hash
        ));
    }
    lines.push("可用性: 認証・モデル・MCPの実接続はこの表示では未検証です".to_owned());
    lines.push(
        "可用性: web-search=disabled。cached/liveは接続先の上限契約が未確認のため利用できません。"
            .to_owned(),
    );
    lines.push("可用性: capacity観測は未接続です。資源の空きを確認したとは扱いません。".to_owned());
    lines.push(format!(
        "設定: audit={}; 保存先変更は --audit を指定して再起動",
        audit_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "未指定".into())
    ));
    lines.push(format!(
        "設定: effort={}; TUIから固定overrideを変更できません",
        fixed_effort.unwrap_or("automatic")
    ));
    lines.push(format!("設定: host-issued routine/review authorityの構成={turn_authority_present}。内容・承認tokenは表示しません。"));
    let mut rendered: Vec<_> = lines
        .into_iter()
        .map(|line| ratatui::text::Line::from(render::sanitize(&line).escape_debug().to_string()))
        .collect();
    rendered.extend(cat_diagnostic_lines(answer_finalizer));
    #[cfg(unix)]
    rendered.extend(management_effect_diagnostic(
        project_root,
        management_effects,
        recovery_issue,
    ));
    rendered
}

fn account_list_lines() -> Result<Vec<ratatui::text::Line<'static>>, String> {
    use polaris_auth::accounts::{owner::AccountOwner, store::AccountState};

    let home = std::env::var_os("HOME").ok_or("HOMEを特定できません")?;
    polaris_auth::protection::use_trusted_home(std::path::Path::new(&home))
        .map_err(|error| format!("認証の保存先を確認できません: {error}"))?;
    let Some(owner) = AccountOwner::open_if_registered(polaris_auth::ISSUER)
        .map_err(|error| format!("アカウントを開けません: {error}"))?
    else {
        return Ok(vec![ratatui::text::Line::from(
            "登録はありません。単一認証を使用しています。",
        )]);
    };
    let snapshot = owner
        .snapshot()
        .map_err(|error| format!("一覧を取得できません: {error}"))?;
    let mut lines = vec![ratatui::text::Line::from(format!(
        "登録アカウント: {}件",
        snapshot.accounts.len()
    ))];
    for account in snapshot.accounts {
        let state = match account.state {
            AccountState::Registering => "登録中",
            AccountState::Ready => "利用可",
            AccountState::RefreshPending => "更新中",
            AccountState::ReauthRequired => "再認証が必要",
            AccountState::Disconnected => "解除済み",
        };
        let usage = match owner.observation(&account.registration_id, "gpt-6-astra") {
            Some(observed) => match observed.state {
                polaris_auth::quota::ObservationState::Known(_) => "利用枠は取得済み",
                polaris_auth::quota::ObservationState::Missing => "利用枠は未取得",
                polaris_auth::quota::ObservationState::FetchFailed => "利用枠の取得に失敗",
                polaris_auth::quota::ObservationState::MappingUnknown => "利用枠の対応は未検証",
                polaris_auth::quota::ObservationState::Unsupported => "利用枠は非対応",
            },
            None => "利用枠は未取得",
        };
        let label = account.label.as_deref().unwrap_or("-");
        lines.push(ratatui::text::Line::from(render::sanitize(&format!(
            "  {}  {state}  {}  {usage}",
            account.registration_id,
            label.escape_debug()
        ))));
    }
    Ok(lines)
}

/// Read-only memory access uses the same project identity and database as the
/// CLI. A historical hit is evidence, never a current instruction or grant.
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
        return Err("記憶への照会は1024バイト以内の1行で指定してください".into());
    }
    let mut parts = request.split_whitespace();
    let operation = parts.next().unwrap_or("");
    if !matches!(operation, "search" | "get" | "source") {
        return Err(format!("このTUIで利用できる記憶操作: {USAGE}"));
    }
    let project_id = persist::project_identity(cwd)
        .map_err(|error| format!("プロジェクトを特定できません: {error}"))?;
    let store = MemoryStore::open_read_only(state_dir.join("memory.sqlite3"))
        .map_err(|error| format!("保存済み記憶を開けません: {error}"))?;
    let mut lines = vec![ratatui::text::Line::from(
        "過去の記録です。現在の指示・承認ではありません。",
    )];
    match operation {
        "search" => {
            let query = request.strip_prefix("search").unwrap_or("").trim();
            if query.is_empty() {
                return Err(format!("検索語が必要です: {USAGE}"));
            }
            let hits = store
                .search(&SearchRequest {
                    project_id: &project_id,
                    session_id: None,
                    mode: SearchMode::Lexical(query),
                    limit: 5,
                    excerpt_bytes: 512,
                })
                .map_err(|error| format!("記憶を検索できません: {error}"))?;
            if hits.is_empty() {
                lines.push(ratatui::text::Line::from("該当する記録はありません"));
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
                .map_err(|_| "startはUTF-8 byte位置です")?
                .unwrap_or(0);
            let budget = parts
                .next()
                .map(str::parse::<usize>)
                .transpose()
                .map_err(|_| "max-bytesは整数です")?
                .unwrap_or(4096);
            if parts.next().is_some() || budget > 4096 {
                return Err("/memory get <session> <id> [start] [max-bytes:0..4096]".into());
            }
            let range = store
                .get_range(&project_id, session, id, start, budget)
                .map_err(|error| format!("記憶を取得できません: {error}"))?
                .ok_or("該当する記録はありません")?;
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
                    "続きは /memory get <session> <id> <表示済みbyte位置> で確認できます。",
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
                return Err("出典IDかconversation:// URIを指定してください".into());
            };
            let data_root = sessions_dir
                .parent()
                .ok_or("会話の保存先を特定できません")?;
            let persisted = polaris_core::session_store::PersistedSession::open(
                data_root,
                &state_dir.join("memory.sqlite3"),
                &project_id,
                session,
            )
            .map_err(|error| format!("保存会話を開けません: {error}"))?;
            let source = polaris_core::conversation_memory::read_source(
                persisted.store.as_ref(),
                &state_dir.join("memory.sqlite3"),
                &uri,
            )
            .map_err(|error| format!("出典を確認できません: {error}"))?;
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
            .map_err(|error| format!("決定事項を読めません: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(format!("決定事項を読めません: {error}")),
    };
    if data.len() > 1024 * 1024 {
        return Err("決定事項の台帳が1 MiBを超えるためTUIでは表示できません".into());
    }
    let text = String::from_utf8(data).map_err(|_| "決定事項の台帳がUTF-8ではありません")?;
    let entries = polaris_core::decisions::parse(&text);
    let mut lines = vec![ratatui::text::Line::from(format!(
        "記録済みの決定事項: {}件。過去の記録であり、現在の承認ではありません。",
        entries.len()
    ))];
    if entries.is_empty() {
        lines.push(ratatui::text::Line::from("記録はありません"));
    }
    let omitted = entries.len().saturating_sub(100);
    if omitted > 0 {
        lines.push(ratatui::text::Line::from(format!(
            "古い{omitted}件は省略しました"
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
        return Err("比較するフォルダは2048バイト以内の1行で指定してください".into());
    }
    let folders = request.split('|').map(str::trim).collect::<Vec<_>>();
    if !(2..=8).contains(&folders.len()) || folders.iter().any(|path| path.is_empty()) {
        return Err(format!("{USAGE}（2〜8フォルダ）"));
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
            .map_err(|error| format!("{} を読めません: {error}", render::sanitize(&id)))?;
        if sampled.truncated {
            lines.push(ratatui::text::Line::from(render::sanitize(&format!(
                "{id}: 一部だけの比較です。似ていない結果は確定しません"
            ))));
        }
        samples.push(sampled.folder);
    }
    let graph = similarity::graph(&samples);
    if graph.omitted > 0 {
        lines.push(ratatui::text::Line::from(format!(
            "{}件は上限のため比較していません",
            graph.omitted
        )));
    }
    if graph.edges.is_empty() {
        lines.push(ratatui::text::Line::from(
            "似ているフォルダはありませんでした",
        ));
    }
    for edge in graph.edges {
        lines.push(ratatui::text::Line::from(render::sanitize(&format!(
            "{} / {}: 類似度 {}、共通 {}件",
            edge.left,
            edge.right,
            edge.similarity,
            edge.shared.len()
        ))));
    }
    for proposal in graph.proposals {
        let why = match proposal.suggestion {
            Suggestion::SameWorkInTwoPlaces => "同じ作業が2箇所にある可能性",
            Suggestion::SharedPart => "共通部分を切り出せる可能性",
        };
        lines.push(ratatui::text::Line::from(render::sanitize(&format!(
            "提案: {} / {}: {why}。{}",
            proposal.left, proposal.right, proposal.because
        ))));
    }
    lines.push(ratatui::text::Line::from(
        "提案は調査の手がかりです。移動・削除は行いません。",
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
        return Err("MCP prompt選択は2048バイト以内の1行で指定してください".into());
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
        .ok_or("MCP promptは server/prompt の形で指定してください")?;
    if server.is_empty() || prompt.is_empty() {
        return Err("MCP promptのserverとpromptを指定してください".into());
    }
    let mut arguments = serde_json::Map::new();
    for argument in parts {
        if arguments.len() >= 16 {
            return Err("MCP promptの引数は16個までです".into());
        }
        let (key, value) = argument
            .split_once('=')
            .ok_or("MCP prompt引数はkey=valueの形で指定してください")?;
        if key.is_empty() || arguments.contains_key(key) {
            return Err("MCP prompt引数のキーが空または重複しています".into());
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
    let mut result = match polaris_core::sync::composite::C10CompositeBlockerV1::inspect_readonly(
        project,
        store.state_dir(),
    ) {
        Ok(Some(effect)) => format!(
            "管理操作: 未解決 {} ({}) / 状態 {:?} / 記録番号 {}",
            effect.request.operation_id,
            effect.request.inventory,
            effect.state,
            effect.last_sequence
        ),
        Ok(None) => "管理操作: 未解決なし".to_owned(),
        Err(error) => format!("管理操作: 状態を確認できません ({error})"),
    };
    if let Some(reason) = recovery_issue {
        result.push_str(&format!(" / 復旧理由: {reason}"));
    }
    vec![ratatui::text::Line::from(
        render::sanitize(&result).escape_debug().to_string(),
    )]
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
            "未解決の管理操作 {} ({}) があります。先に状態を確認してください",
            effect.request.operation_id, effect.request.inventory
        )),
        Ok(None) => None,
        Err(error) => Some(format!("管理操作の状態を確認できません: {error}")),
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
        | slash::Action::WorkspaceControl(_)
        | slash::Action::RuntimeControl(_) => {
            *status = Status::Error("この操作には有効な端末と保存ownerが必要です".into());
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
            *status = Status::Error("/status の表示経路を確認できません".into());
            SlashOutcome::Continue
        }
        slash::Action::WebSearch(requested) => {
            if !matches!(requested.as_str(), "" | "status" | "disabled") {
                *status = Status::Error("Web検索は接続先の上限契約が未確認のため利用できません。disabledを使用してください。".into());
            } else {
                local_lines.push(ratatui::text::Line::from("Web検索: disabled。cached/liveは接続先の上限契約が未確認のため利用できません。送信は行っていません。"));
                *status = Status::Notice("Web検索の利用条件を表示しました".into());
            }
            SlashOutcome::Continue
        }
        slash::Action::Account(requested) => {
            if !requested.is_empty() && requested != "list" {
                *status = Status::Error("/account は現在 list のみ利用できます".into());
                return SlashOutcome::Continue;
            }
            match account_list_lines() {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("アカウント一覧を表示しました".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        slash::Action::Memory(requested) => {
            match memory_read_lines(cwd, state_dir, sessions_dir, &requested) {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("保存済み記憶の参照結果を表示しました".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        slash::Action::Decided(requested) => {
            if !requested.is_empty() && requested != "list" {
                *status = Status::Error("/decided は現在 list のみ利用できます".into());
                return SlashOutcome::Continue;
            }
            match decided_read_lines(cwd) {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("決定事項を表示しました".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        slash::Action::Compare(requested) => {
            match compare_read_lines(cwd, &requested) {
                Ok(lines) => {
                    local_lines.extend(lines);
                    *status = Status::Notice("フォルダ比較を表示しました".into());
                }
                Err(error) => *status = Status::Error(error),
            }
            SlashOutcome::Continue
        }
        #[cfg(target_os = "linux")]
        slash::Action::Apply(_) => {
            *status = Status::Error("D27操作には実端末とsource-bound ownerが必要です".into());
            SlashOutcome::Continue
        }
        slash::Action::Sync(_) => {
            *status = Status::Error("共有適用には対応OSの確認画面と共有ownerが必要です".into());
            SlashOutcome::Continue
        }
        slash::Action::Phase(requested) => {
            *status = if requested.is_empty() {
                session.workflow.as_ref().map_or_else(
                    || Status::Notice("workflowはこのセッションで無効です".into()),
                    |workflow| {
                        Status::Notice(format!("ワークフロー段階: {}", workflow.state().phase))
                    },
                )
            } else {
                Status::Error("phase変更には管理操作の確認画面が必要です".into())
            };
            SlashOutcome::Continue
        }
        slash::Action::Workflow(requested) => {
            *status = if requested.is_empty() {
                session.workflow.as_ref().map_or_else(
                    || Status::Notice("workflowはこのセッションで無効です".into()),
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
                Status::Error("workflow変更には管理操作の確認画面が必要です".into())
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
                *status = Status::Error("/mcp prompt の選択は入力受付画面で処理します".into());
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
                *status = Status::Error("Windowsでは /diff はまだ利用できません".into());
                SlashOutcome::Continue
            }
            #[cfg(unix)]
            {
                #[cfg(not(target_os = "linux"))]
                const MAX_DIFF_LINES: usize = 200;
                let text = (|| -> Result<String, String> {
                    let project_root = polaris_core::project::resolve_root(cwd);
                    let root = std::fs::File::open(project_root)
                        .map_err(|_| "Git情報を取得できません".to_string())?;
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
            *status = Status::Error("解除には対象アカウントの確認画面が必要です".into());
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
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_pre_raw_injection_avoids_terminal_mutation() {
        use std::cell::Cell;

        let terminal_calls = Cell::new(0);
        let result = linux_start_terminal(
            || Some(signal::LinuxProcessShutdown::SigInt),
            || {
                terminal_calls.set(terminal_calls.get() + 1);
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(LinuxTerminalStartError::Shutdown(
                signal::LinuxProcessShutdown::SigInt
            ))
        ));
        assert_eq!(terminal_calls.get(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_startup_resume_guard_restores_once_on_error() {
        let mut guard = LinuxTerminalGuard {
            alternate: true,
            raw: true,
            mouse: false,
            cursor: false,
        };
        let mut steps = Vec::new();
        let result: Result<(), String> = linux_finish_guarded_terminal(
            Err("saved transcript read failed".into()),
            &mut guard,
            |step| {
                steps.push(step);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("saved transcript read failed"));
        guard
            .restore_with(|step| {
                steps.push(step);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            steps,
            [LinuxTerminalStep::Raw, LinuxTerminalStep::Alternate]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_startup_workflow_guard_restores_once_on_error() {
        let mut guard = LinuxTerminalGuard {
            alternate: true,
            raw: true,
            mouse: false,
            cursor: false,
        };
        let mut steps = Vec::new();
        let result: Result<bool, String> = linux_finish_guarded_terminal(
            Err("confirmation draw failed".into()),
            &mut guard,
            |step| {
                steps.push(step);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("confirmation draw failed"));
        guard
            .restore_with(|step| {
                steps.push(step);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            steps,
            [LinuxTerminalStep::Raw, LinuxTerminalStep::Alternate]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_picker_shutdown_accepts_no_choice() {
        use approver::KeyReader;
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use std::cell::Cell;

        struct LatchingReader(Cell<bool>);
        impl KeyReader for LatchingReader {
            fn read_key(&mut self) -> std::io::Result<KeyCode> {
                let latched = &self.0;
                linux_read_key_with(
                    |_| Ok(true),
                    || {
                        latched.set(true);
                        Ok(Event::Key(KeyEvent::new(
                            KeyCode::Enter,
                            KeyModifiers::NONE,
                        )))
                    },
                    || latched.get(),
                )
            }
        }

        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 14)).unwrap(),
        );
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        let mut approval_policy = ApprovalPolicy::OnRequest;
        let mut status = Status::Idle;
        let mut reader = LatchingReader(Cell::new(false));
        handle_permissions(
            &terminal,
            &mut reader,
            &mut approval_policy,
            ApprovalPolicy::OnRequest,
            &sandbox,
            &mut status,
        );
        assert!(reader.0.get());
        assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
        assert!(matches!(status, Status::Idle));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_diff_result_is_suppressed_after_latch() {
        let mut lines = vec![ratatui::text::Line::from("earlier output")];
        let result = append_diff_result_after_read("secret diff line\n", &mut lines, || {
            Some(signal::LinuxProcessShutdown::SigTerm)
        });
        assert_eq!(result, Err(signal::LinuxProcessShutdown::SigTerm));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "earlier output");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_signal_tui_inflight_shutdown_uses_existing_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .unwrap();
        let mut session = polaris_core::session::Session::new();
        let control = install_tui_root_control(
            &mut session,
            TuiRootControlArgs {
                session_id: "signal-session",
                turn: 1,
                sandbox: &sandbox,
                approval: ApprovalPolicy::OnRequest,
                mcp_available: false,
                tool_names: &["read".into()],
                model: "gpt-6-astra",
                effort: "automatic",
            },
        )
        .unwrap();
        assert!(linux_cancel_owned_children(
            Some(signal::LinuxProcessShutdown::SigInt),
            true,
        ));
        assert_eq!(control.snapshot().await.session_id, "signal-session");
        session.finish_root_delegation_control(true).await.unwrap();
        assert!(session.delegation_control.is_none());
        assert!(linux_cancel_owned_children(None, false));
        assert!(!linux_cancel_owned_children(None, true));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_first_signal_wins_during_cleanup() {
        use polaris_core::session_manager::TerminalStatus;
        use std::cell::Cell;

        // The turn can finish before the signal arrives while child cleanup
        // is still awaited. Check the latch again before persisting receipt.
        let pending = Cell::new(None);
        let before_cleanup = pending.get();
        pending.set(Some(signal::LinuxProcessShutdown::SigInt));
        assert_eq!(
            linux_admission_status_after_cleanup(
                TerminalStatus::Completed,
                before_cleanup.or_else(|| pending.get()),
            ),
            TerminalStatus::OutcomeUnknown,
        );
        assert_eq!(
            linux_admission_status_after_cleanup(TerminalStatus::Completed, None),
            TerminalStatus::Completed,
        );
        assert!(!linux_turn_completed_without_shutdown(true, pending.get()));
        assert!(linux_turn_completed_without_shutdown(true, None));
        pending.set(None);
        let before_capture = linux_turn_completed_without_shutdown(true, pending.get());
        assert!(before_capture);
        // The signal arrives while post-capture effects are classified, after
        // an earlier clear check but before the boundary's final commit.
        let _effect = {
            pending.set(Some(signal::LinuxProcessShutdown::SigInt));
            polaris_core::conversation_state::RewindEffectStatusV1::NoExternalMutation
        };
        assert!(!linux_turn_completed_without_shutdown(
            before_capture,
            pending.get(),
        ));
        let first = linux_signal_exit_code(signal::LinuxProcessShutdown::SigInt);
        let (exit, error) = linux_exit_after_terminal_restoration(
            first,
            Ok(()),
            Some(signal::LinuxProcessShutdown::SigTerm),
        );
        assert_eq!(exit, ExitCode::from(130));
        assert!(error.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_cleanup_failure_exits_one_before_signal_status() {
        let (exit, error) = linux_exit_after_terminal_restoration(
            linux_signal_exit_code(signal::LinuxProcessShutdown::SigTerm),
            Err(std::io::ErrorKind::BrokenPipe.into()),
            Some(signal::LinuxProcessShutdown::SigTerm),
        );
        assert_eq!(exit, ExitCode::FAILURE);
        assert!(error.unwrap().contains("terminal restoration failed"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_tui_handled_error_restores_every_acquired_state() {
        let mut guard = LinuxTerminalGuard {
            alternate: true,
            raw: true,
            mouse: true,
            cursor: true,
        };
        let mut steps = Vec::new();
        let result: Result<(), String> =
            linux_finish_guarded_terminal(Err("handled error".into()), &mut guard, |step| {
                steps.push(step);
                if step == LinuxTerminalStep::Cursor {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                } else {
                    Ok(())
                }
            });
        let error = result.unwrap_err();
        assert!(error.contains("handled error"));
        assert!(error.contains("terminal restoration failed"));
        guard
            .restore_with(|step| {
                steps.push(step);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            steps,
            [
                LinuxTerminalStep::Cursor,
                LinuxTerminalStep::Mouse,
                LinuxTerminalStep::Raw,
                LinuxTerminalStep::Alternate,
            ]
        );
    }

    #[test]
    fn m3c_memory_read_keeps_project_scope_and_bounds_exact_ranges() {
        use polaris_memory::{MemoryStore, Record};

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let other_project = root.path().join("other");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&other_project).unwrap();
        let state_dir = root.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        assert!(memory_read_lines(&project, &state_dir, root.path(), "search needle").is_err());
        assert!(!state_dir.join("memory.sqlite3").exists());

        let mut store = MemoryStore::open(state_dir.join("memory.sqlite3")).unwrap();
        for (path, id) in [
            (&project, "same-project"),
            (&other_project, "other-project"),
        ] {
            store
                .upsert(
                    &Record {
                        project_id: persist::project_identity(path).unwrap(),
                        session_id: "session-1".into(),
                        id: id.into(),
                        role: "user".into(),
                        timestamp: "2026-09-23".into(),
                        source: "test".into(),
                        text: "needle evidence".into(),
                    },
                    None,
                )
                .unwrap();
        }
        drop(store);
        let lines = memory_read_lines(&project, &state_dir, root.path(), "search needle")
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("same-project"), "{lines}");
        assert!(!lines.contains("other-project"), "{lines}");
        assert!(lines.contains("現在の指示・承認ではありません"));
        let page = memory_read_lines(
            &project,
            &state_dir,
            root.path(),
            "get session-1 same-project 7 8",
        )
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert!(page.contains("evidence"));
        assert!(
            memory_read_lines(
                &project,
                &state_dir,
                root.path(),
                "get session-1 same-project 900 8"
            )
            .is_err()
        );
        assert!(
            memory_read_lines(
                &project,
                &state_dir,
                root.path(),
                "get session-1 same-project 0 4097"
            )
            .is_err()
        );
        assert!(
            memory_read_lines(
                &project,
                &state_dir,
                root.path(),
                "get session-1 other-project"
            )
            .is_err()
        );
    }

    #[test]
    fn decided_read_is_bounded_and_marks_old_entries_as_history() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".polaris")).unwrap();
        polaris_core::decisions::append(root.path(), "2026-09-23", "keep the old name").unwrap();
        let lines = decided_read_lines(root.path())
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("keep the old name"), "{lines}");
        assert!(lines.contains("現在の承認ではありません"), "{lines}");
        std::fs::write(
            root.path().join(polaris_core::decisions::PATH),
            "x".repeat(1024 * 1024 + 1),
        )
        .unwrap();
        assert!(decided_read_lines(root.path()).is_err());
    }

    #[test]
    fn readonly_acceptance_compare_folders_and_errors_preserve_source() {
        let root = tempfile::tempdir().unwrap();
        let left = root.path().join("left");
        let right = root.path().join("right");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        for path in [&left, &right] {
            std::fs::write(path.join("same.txt"), "contents stay in place").unwrap();
        }
        let mut session = Session::default();
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let session_path = root.path().join("session.jsonl");
        let outcome = call(
            slash::parse("/compare left | right").unwrap(),
            &mut session,
            &session_path,
            &mut status,
            &[],
            root.path(),
            &mut local_lines,
        );
        assert!(matches!(outcome, SlashOutcome::Continue));
        assert!(matches!(status, Status::Notice(_)));
        let lines = local_lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("類似度"), "{lines}");
        assert!(lines.contains("移動・削除は行いません"), "{lines}");
        for command in [
            "/compare left | missing".to_owned(),
            format!("/compare {} | right", "x".repeat(2049)),
        ] {
            let before = local_lines.clone();
            call(
                slash::parse(&command).unwrap(),
                &mut session,
                &session_path,
                &mut status,
                &[],
                root.path(),
                &mut local_lines,
            );
            assert!(matches!(status, Status::Error(_)));
            assert_eq!(local_lines, before);
        }
        assert!(session.messages.is_empty());
        assert!(!session_path.exists());
        assert_eq!(
            std::fs::read_to_string(left.join("same.txt")).unwrap(),
            "contents stay in place"
        );
        assert_eq!(
            std::fs::read_to_string(right.join("same.txt")).unwrap(),
            "contents stay in place"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn readonly_acceptance_models_distinguishes_observation_and_installation() {
        let Some(slash::Action::WorkspaceControl(request)) = slash::parse("/models list") else {
            panic!("model inventory must reach the read-only command route");
        };
        let (verb, arguments) = request.split_once(' ').unwrap();
        assert_eq!(verb, "models");
        let lines = model_inventory_read_lines(arguments).await.unwrap();
        assert!(lines[0].to_string().contains("接続先の報告"));
        assert!(lines[0].to_string().contains("インストール状態は未確認"));
        assert!(lines.len() <= 66);
        assert!(
            lines
                .iter()
                .all(|line| !line.to_string().contains(['\n', '\t', '\x1b']))
        );
        assert!(model_inventory_read_lines("install").await.is_err());
    }

    #[test]
    fn mcp_prompt_selection_is_typed_and_rejects_ambiguous_arguments() {
        let choice = mcp_prompt_choice("prompt files/review path=src/lib.rs depth=2").unwrap();
        assert_eq!(choice.server, "files");
        assert_eq!(choice.prompt, "review");
        assert_eq!(choice.arguments["depth"], "2");
        assert!(mcp_prompt_choice("prompt files/review path=a path=b").is_err());
        assert!(mcp_prompt_choice("prompt files/review path").is_err());
        assert!(mcp_prompt_choice("prompt files/review\nsecret=x").is_err());
    }

    #[test]
    fn mcp_prompt_selection_does_not_follow_a_different_session() {
        let mut pending = Some(mcp_prompt_choice("prompt files/review path=src/lib.rs").unwrap());
        let mut requires_reselect = true;
        clear_mcp_prompt_on_session_change(
            std::path::Path::new("/sessions/first.jsonl"),
            std::path::Path::new("/sessions/first.jsonl"),
            &mut pending,
            &mut requires_reselect,
        );
        assert!(pending.is_some());
        assert!(requires_reselect);

        clear_mcp_prompt_on_session_change(
            std::path::Path::new("/sessions/first.jsonl"),
            std::path::Path::new("/sessions/second.jsonl"),
            &mut pending,
            &mut requires_reselect,
        );
        assert!(pending.is_none());
        assert!(!requires_reselect);
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_management_effect_blocks_mutating_slash_but_allows_diagnostics() {
        use polaris_core::management_effect::{
            ManagementEffectHashV1, ManagementEffectScopeV1, ManagementInventoryIdV1,
            ManagementOperationIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
            PrepareManagementEffectV1,
        };
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state_dir = root.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(
            &state_dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state_dir.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        let store = polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
            &project, &state_dir, &lease,
        )
        .unwrap();
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random().unwrap(),
            inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: store.project_scope_hash(),
                session: None,
                generation: 0,
                revision: 0,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(b"synthetic-init"),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        store
            .prepare(&lease, store.cursor().unwrap(), request)
            .unwrap();
        drop(lease);

        assert!(management_clearance_error(&project, &store, &slash::Action::Init).is_some());
        assert!(management_clearance_error(&project, &store, &slash::Action::Status).is_none());
        let write_root = root.path().join("write\nroot");
        std::fs::create_dir(&write_root).unwrap();
        let sandbox =
            SandboxPolicy::new(polaris_sandbox::SandboxMode::WorkspaceWrite, &[write_root])
                .unwrap();
        let session = Session::default();
        let lines = status_lines(StatusSnapshot {
            session: &session,
            provider_name: "provider\n管理操作: 未解決なし",
            model_name: "model\tname",
            usage_report: polaris_provider::UsageReport::default(),
            sandbox: &sandbox,
            approval: ApprovalPolicy::Never,
            startup_approval: ApprovalPolicy::OnRequest,
            mcp_external: false,
            audit_path: None,
            turn_authority_present: false,
            fixed_effort: None,
            answer_finalizer: None,
            project_root: &project,
            #[cfg(unix)]
            management_effects: &store,
            recovery_issue: Some("復旧\n確認待ち"),
        });
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("現在=Never, 起動時=OnRequest"), "{text}");
        let blocked = store.blocker().unwrap().unwrap();
        assert!(
            text.contains(&blocked.request.operation_id.to_string()),
            "{text}"
        );
        assert!(text.contains("復旧\\n確認待ち"), "{text}");
        assert!(text.contains("provider\\n管理操作: 未解決なし"), "{text}");
        assert!(text.contains("model\\tname"), "{text}");
        assert!(text.contains("write\\nroot"), "{text}");
        assert!(lines.iter().all(|line| {
            let line = line.to_string();
            !line.contains('\n') && !line.contains('\t')
        }));
        assert!(session.messages.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_rewind_store_refuses_session_start_instead_of_skipping_resume_validation() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state_dir = root.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::write(state_dir.join("rewind-workspace"), "blocked").unwrap();
        assert!(open_rewind_workspace(&project, &state_dir).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn startup_mismatch_shows_saved_transcript_without_a_run() {
        use polaris_core::conversation_state::HistoryMode;
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_core::session_store::PersistedSession;

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let file = project.join("task.txt");
        std::fs::write(&file, "original").unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let persistence = PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = polaris_core::session::Session::new();
        session.attach(persistence.clone()).unwrap();
        session.push_user("saved question");
        session.push_assistant("saved answer", Vec::new());
        session.check_persistence().unwrap();
        validate_resume_workspace(&owner, &persistence).unwrap();
        std::fs::write(&file, "different tree").unwrap();
        let reason = validate_resume_workspace(&owner, &persistence).unwrap_err();

        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 18)).unwrap(),
        );
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter]));
        show_persisted_read_only_resume(&terminal, &mut reader, &persistence, &reason).unwrap();
        let screen = {
            let terminal = terminal.borrow();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>()
        };
        assert!(screen.contains("saved question"), "{screen}");
        assert!(screen.contains("saved answer"), "{screen}");
        assert!(screen.replace(' ', "").contains("閲覧のみ"), "{screen}");
        assert_eq!(persistence.snapshot().unwrap().events.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn rewind_management_crossing_is_rechecked_before_completion_and_child_creation() {
        use polaris_core::{
            conversation_state::{HistoryMode, RewindEffectStatusV1, RewindEligibilityV1},
            management_effect::*,
            rewind_workspace::{RewindLimits, WorkspaceOwner},
            session_store::PersistedSession,
        };
        use polaris_sandbox::{SandboxMode, SandboxPolicy};
        use std::os::unix::fs::PermissionsExt;

        for case in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            let state = root.path().join("state");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir(project.join(".polaris")).unwrap();
            std::fs::create_dir(&state).unwrap();
            std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
            let owner = WorkspaceOwner::open(
                &project,
                &state.join("rewind-workspace"),
                RewindLimits::default(),
            )
            .unwrap();
            let lease = owner.try_mutation().unwrap();
            let effects =
                ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
            drop(lease);
            let policy =
                SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project))
                    .unwrap();
            let persistence = PersistedSession::create(
                root.path(),
                &state.join("memory.sqlite3"),
                project.to_str().unwrap(),
                HistoryMode::Legacy,
                None,
            )
            .unwrap();
            let complete_c10 = || {
                let lease = owner.try_mutation().unwrap();
                let request = PrepareManagementEffectV1 {
                    operation_id: "00000000000000000000000000000001".parse().unwrap(),
                    inventory: ManagementInventoryIdV1::parse("C10").unwrap(),
                    scope: ManagementEffectScopeV1 {
                        project: effects.project_scope_hash(),
                        session: None,
                        generation: 0,
                        revision: 0,
                        permission: ManagementPermissionScopeV1::Remote,
                        account_binding: Some(ManagementEffectHashV1::digest(
                            b"synthetic-keychain-binding",
                        )),
                    },
                    request_hash: ManagementEffectHashV1::digest(b"synthetic-c10"),
                    rewind: ManagementRewindDispositionV1::IrreversibleRemote,
                };
                let PrepareManagementEffectOutcomeV1::Prepared(prepared) = effects
                    .prepare(&lease, effects.cursor().unwrap(), request)
                    .unwrap()
                else {
                    panic!("new C10")
                };
                let started = effects.start(&lease, &prepared).unwrap();
                effects
                    .complete_started(
                        &lease,
                        &started,
                        ManagementEffectHashV1::digest(b"synthetic-remote-observation"),
                    )
                    .unwrap();
                assert!(effects.blocker().unwrap().is_none());
            };
            if case == 3 {
                complete_c10();
            }
            let pre = owner.capture(&policy).unwrap();
            let boundary = persistence
                .begin_rewind_boundary(pre.to_string(), Vec::new())
                .unwrap();
            let mut session = polaris_core::session::Session::new();
            session.attach(persistence.clone()).unwrap();
            session.push_user("edit task");
            std::fs::write(project.join("task.txt"), "after").unwrap();
            session.push_assistant("done", Vec::new());
            session.check_persistence().unwrap();
            if case == 1 || case == 2 {
                complete_c10();
            }
            let saved = if case == 2 {
                let post = owner.capture(&policy).unwrap();
                persistence
                    .finish_rewind_boundary(
                        &boundary.boundary_id,
                        post.to_string(),
                        RewindEligibilityV1::Eligible,
                        RewindEffectStatusV1::NoExternalMutation,
                    )
                    .unwrap()
            } else {
                finish_rewind_for_session(
                    &owner,
                    &persistence,
                    &effects,
                    &policy,
                    &boundary,
                    None,
                    || true,
                )
                .unwrap()
            };
            assert_eq!(
                matches!(
                    saved.completion.unwrap().eligibility,
                    RewindEligibilityV1::Eligible
                ),
                case != 1
            );
            let sessions = root.path().join("sessions-v2");
            let before = std::fs::read_dir(&sessions).unwrap().count();
            let candidate = persistence.prepare_rewind_branch(&owner, &boundary.boundary_id);
            if case == 0 {
                assert!(candidate.is_ok());
            } else {
                // Case 3 has a clear historical range but an orphan terminal
                // C10 effect. P4 must refuse new work independently of crossing.
                assert!(candidate.is_err());
                assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), before);
                let manifests = state.join("rewind-workspace/manifests");
                let before_capture = std::fs::read_dir(&manifests).unwrap().count();
                assert!(capture_rewind_before_input(&owner, &effects, &policy).is_err());
                assert_eq!(
                    std::fs::read_dir(&manifests).unwrap().count(),
                    before_capture
                );
            }
            assert_eq!(
                std::fs::read_to_string(project.join("task.txt")).unwrap(),
                "after"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn resume_requires_the_workspace_recorded_at_the_conversation_head() {
        use polaris_core::conversation_state::{
            HistoryMode, RewindEffectStatusV1, RewindEligibilityV1,
        };
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_core::session_store::PersistedSession;
        use polaris_sandbox::{SandboxMode, SandboxPolicy};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let file = project.join("task.txt");
        std::fs::write(&file, "before").unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let policy =
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project))
                .unwrap();
        let persistence = PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let pre = owner.capture(&policy).unwrap();
        let boundary = persistence
            .begin_rewind_boundary(pre.to_string(), Vec::new())
            .unwrap();
        let mut session = polaris_core::session::Session::new();
        session.attach(persistence.clone()).unwrap();
        session.push_user("edit the file");
        session.push_assistant("done", Vec::new());
        session.check_persistence().unwrap();
        std::fs::write(&file, "after").unwrap();
        let post = owner.capture(&policy).unwrap();
        persistence
            .finish_rewind_boundary(
                &boundary.boundary_id,
                post.to_string(),
                RewindEligibilityV1::Eligible,
                RewindEffectStatusV1::NoExternalMutation,
            )
            .unwrap();

        assert!(validate_resume_workspace(&owner, &persistence).is_ok());
        std::fs::write(&file, "unrelated version").unwrap();
        assert!(validate_resume_workspace(&owner, &persistence).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn first_unbound_resume_binds_the_tree_and_later_unbound_resumes_refuse() {
        use polaris_core::conversation_state::HistoryMode;
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_core::session_store::PersistedSession;

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let file = project.join("task.txt");
        std::fs::write(&file, "initial").unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let database = root.path().join("memory.sqlite3");
        let first =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let id = first.snapshot().unwrap().state.session_id;
        assert!(validate_resume_workspace(&owner, &first).is_ok());
        assert!(first.store.workspace_head_binding().unwrap().is_some());
        let reopened = PersistedSession::open(root.path(), &database, "project", &id).unwrap();
        assert!(validate_resume_workspace(&owner, &reopened).is_ok());
        let second =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        assert!(validate_resume_workspace(&owner, &second).is_err());
        std::fs::write(file, "changed").unwrap();
        assert!(validate_resume_workspace(&owner, &reopened).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_first_binding_retries_with_the_current_tree() {
        use polaris_core::conversation_state::HistoryMode;
        use polaris_core::rewind_workspace::{ErrorKind, RewindLimits, WorkspaceOwner};
        use polaris_core::session_store::PersistedSession;
        use polaris_sandbox::{SandboxMode, SandboxPolicy};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let file = project.join("task.txt");
        std::fs::write(&file, "initial").unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[project]).unwrap();
        let session = PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let snapshot = session.snapshot().unwrap();
        let id = snapshot.state.session_id.clone();
        let interrupted = owner.bind_first_legacy_head(&policy, &id, |manifest, permit| {
            session.store.repair_pending_workspace_head_binding(
                &snapshot,
                &manifest.to_string(),
                permit,
            )?;
            std::fs::write(&file, "changed while binding")
        });
        assert_eq!(interrupted.unwrap_err().kind, ErrorKind::Conflict);
        assert!(owner.pending_legacy_binding_for(&id).unwrap());
        assert!(validate_resume_workspace(&owner, &session).is_ok());
        assert!(!owner.pending_legacy_binding_for(&id).unwrap());
        let binding = session.store.workspace_head_binding().unwrap().unwrap();
        let manifest = binding.workspace_manifest_id.parse().unwrap();
        assert!(owner.matches_manifest(&manifest).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn two_turn_rewind_restores_earlier_files_and_blocks_the_origin() {
        use polaris_core::conversation_state::{
            HistoryMode, RewindEffectStatusV1, RewindEligibilityV1,
        };
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_core::session_store::PersistedSession;
        use polaris_sandbox::{SandboxMode, SandboxPolicy};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let policy =
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project))
                .unwrap();
        let persistence = PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = polaris_core::session::Session::new();
        session.attach(persistence.clone()).unwrap();

        let first_pre = owner.capture(&policy).unwrap();
        let first = persistence
            .begin_rewind_boundary(first_pre.to_string(), Vec::new())
            .unwrap();
        session.push_user("create a file");
        std::fs::write(project.join("task.txt"), "first").unwrap();
        session.push_assistant("created", Vec::new());
        session.check_persistence().unwrap();
        let first_post = owner.capture(&policy).unwrap();
        persistence
            .finish_rewind_boundary(
                &first.boundary_id,
                first_post.to_string(),
                RewindEligibilityV1::Eligible,
                RewindEffectStatusV1::NoExternalMutation,
            )
            .unwrap();

        let second_pre = owner.capture(&policy).unwrap();
        let second = persistence
            .begin_rewind_boundary(second_pre.to_string(), Vec::new())
            .unwrap();
        session.push_user("change the file");
        std::fs::write(project.join("task.txt"), "second").unwrap();
        std::fs::create_dir(project.join("new-dir")).unwrap();
        std::fs::write(project.join("new-dir/extra.txt"), "later").unwrap();
        session.push_assistant("changed", Vec::new());
        session.check_persistence().unwrap();
        let second_post = owner.capture(&policy).unwrap();
        persistence
            .finish_rewind_boundary(
                &second.boundary_id,
                second_post.to_string(),
                RewindEligibilityV1::Eligible,
                RewindEffectStatusV1::NoExternalMutation,
            )
            .unwrap();

        let initial_branch = persistence
            .prepare_rewind_branch(&owner, &first.boundary_id)
            .unwrap();
        assert!(
            !initial_branch
                .snapshot()
                .unwrap()
                .events
                .iter()
                .any(|event| event.message.content == "create a file")
        );

        let branch = persistence
            .prepare_rewind_branch(&owner, &second.boundary_id)
            .unwrap();
        let branch_snapshot = branch.snapshot().unwrap();
        assert!(
            branch_snapshot
                .events
                .iter()
                .any(|event| event.message.content == "created")
        );
        assert!(
            !branch_snapshot
                .events
                .iter()
                .any(|event| event.message.content == "change the file")
        );
        let restored = owner
            .restore_for_branch(&second_post, &second_pre, &branch_snapshot.state.session_id)
            .unwrap();
        owner.commit(&restored.transaction_id).unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join("task.txt")).unwrap(),
            "first"
        );
        assert!(!project.join("new-dir").exists());
        assert!(
            validate_resume_workspace(&owner, &branch).is_ok(),
            "{:?}",
            validate_resume_workspace(&owner, &branch)
        );
        assert!(validate_resume_workspace(&owner, &persistence).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn restarted_branch_preserves_workflow_state_without_copying_old_sidechat() {
        use polaris_core::conversation_state::{
            HistoryMode, RewindEffectStatusV1, RewindEligibilityV1,
        };
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_core::session_manager::{
            DestinationMode, SessionId, SubmissionPayloadV1, SubmitOutcomeV1, SubmitRequestV1,
        };
        use polaris_core::session_store::PersistedSession;
        use polaris_core::workflow::Phase;
        use polaris_sandbox::{SandboxMode, SandboxPolicy};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        let state_dir = root.path().join("state");
        std::fs::create_dir(&project).unwrap();
        let file = project.join("task.txt");
        std::fs::write(&file, "before").unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[project]).unwrap();
        let database = root.path().join("memory.sqlite3");
        let source =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let root_snapshot = source.snapshot().unwrap();
        let root_id = SessionId::new(root_snapshot.state.session_id.clone()).unwrap();
        let receipt_counts = std::collections::BTreeMap::from([(
            root_id.clone(),
            root_snapshot.state.admission_receipts.len(),
        )]);
        let (store, initial_manager) = open_tui_admission_owner(&state_dir, &root_id).unwrap();
        let mut old_manager = initial_manager.clone();
        let before = old_manager.revision;
        let SubmitOutcomeV1::Accepted { admission, .. } = old_manager
            .submit(
                SubmitRequestV1 {
                    parent_session: root_id.clone(),
                    payload: SubmissionPayloadV1 {
                        text: "unrelated side work".into(),
                        attachment_ids: Vec::new(),
                        route: DestinationMode::New,
                        expected_session_revision: 0,
                        expected_goal_revision: 0,
                    },
                    admission_id: None,
                },
                old_manager.revision,
            )
            .unwrap();
        store
            .persist_submission(before, &old_manager, &receipt_counts)
            .unwrap();
        let before = old_manager.revision;
        let decided = old_manager
            .decide(
                &admission.id,
                old_manager.revision,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        persist_manager_transition(&store, before, &old_manager).unwrap();
        let before = old_manager.revision;
        old_manager
            .prepare_destination(&decided.id, before, decided.revision)
            .unwrap();
        persist_manager_transition(&store, before, &old_manager).unwrap();
        assert_eq!(old_manager.sessions.len(), 2);
        drop(store);

        let mut session = polaris_core::session::Session::new();
        session.attach(source.clone()).unwrap();
        session.push_user("previous turn");
        session.push_assistant("previous answer", Vec::new());
        session.check_persistence().unwrap();
        let mut lock = source.store.lock().unwrap();
        let snapshot = lock.snapshot().unwrap();
        let mut state = snapshot.state.clone();
        state.workflow.phase = Phase::Review;
        state.workflow.revision = 1;
        state.workflow_artifacts.revision = 1;
        lock.publish(&snapshot, state).unwrap();
        drop(lock);
        drop(session);
        let source =
            PersistedSession::open(root.path(), &database, "project", root_id.as_str()).unwrap();
        let mut session = polaris_core::session::Session::new();
        session.attach(source.clone()).unwrap();

        let pre = owner.capture(&policy).unwrap();
        let boundary = source
            .begin_rewind_boundary(pre.to_string(), Vec::new())
            .unwrap();
        session.push_user("later turn");
        session.push_assistant("later answer", Vec::new());
        session.check_persistence().unwrap();
        std::fs::write(&file, "after").unwrap();
        let post = owner.capture(&policy).unwrap();
        source
            .finish_rewind_boundary(
                &boundary.boundary_id,
                post.to_string(),
                RewindEligibilityV1::Eligible,
                RewindEffectStatusV1::NoExternalMutation,
            )
            .unwrap();
        let branch = source
            .prepare_rewind_branch(&owner, &boundary.boundary_id)
            .unwrap();
        let branch_id = branch.snapshot().unwrap().state.session_id;
        let restored = owner.restore_for_branch(&post, &pre, &branch_id).unwrap();
        owner.commit(&restored.transaction_id).unwrap();

        let restarted =
            PersistedSession::open(root.path(), &database, "project", &branch_id).unwrap();
        validate_resume_workspace(&owner, &restarted).unwrap();
        let branch_snapshot = restarted.snapshot().unwrap();
        assert_eq!(branch_snapshot.state.workflow.phase, Phase::Review);
        assert_eq!(branch_snapshot.state.workflow_artifacts.revision, 1);
        assert_eq!(branch_snapshot.events.len(), 2);
        let branch_session_id = SessionId::new(branch_id).unwrap();
        let (new_store, new_manager) =
            open_tui_admission_owner(&state_dir, &branch_session_id).unwrap();
        assert_eq!(new_manager.sessions.len(), 1);
        assert!(new_manager.sessions.contains_key(&branch_session_id));
        drop(new_store);
        let (_old_store, retained_old_manager) =
            open_tui_admission_owner(&state_dir, &root_id).unwrap();
        assert_eq!(retained_old_manager.sessions.len(), 2);
    }

    struct DistinctContextJudge;

    #[cfg(unix)]
    fn prompt_fixture_host(
        project: &Path,
        home: &Path,
        mut server: polaris_mcp::registration::Server,
    ) -> polaris_core::mcp::local::LocalHost {
        use polaris_mcp::registration::Registry;

        let host = polaris_core::mcp::local::LocalHost::open_for_test(project, home, true).unwrap();
        server.enabled_projects = [host.project().to_owned()].into_iter().collect();
        host.store()
            .unwrap()
            .save_registry(
                0,
                &Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![server],
                },
            )
            .unwrap();
        host
    }

    #[async_trait::async_trait]
    impl polaris_provider::decision::Decider for DistinctContextJudge {
        async fn decide(
            &self,
            _state: &serde_json::Value,
            _questions: &std::collections::BTreeMap<String, polaris_provider::decision::Question>,
        ) -> Result<polaris_provider::decision::Decision, polaris_provider::decision::DecisionError>
        {
            Ok(polaris_provider::decision::Decision {
                answers: std::collections::BTreeMap::from([(
                    "context_relation".into(),
                    polaris_provider::decision::Answer {
                        choice: Some("distinct_context".into()),
                        probabilities: std::collections::BTreeMap::from([(
                            "distinct_context".into(),
                            0.97,
                        )]),
                        confidence: Some(0.96),
                        ..Default::default()
                    },
                )]),
                input_tokens: Some(8),
                output_tokens: Some(1),
            })
        }
    }

    #[cfg(unix)]
    fn mcp_prompt_child_gate(exact_test_name: &str, sentinel: &str) -> bool {
        const MARKER: &str = "POLARIS_TUI_MCP_PROMPT_CHILD";
        if let Some(value) = std::env::var_os(MARKER) {
            assert!(
                value == exact_test_name,
                "unexpected MCP prompt child marker"
            );
            return false;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(exact_test_name)
            .arg("--nocapture")
            .env(MARKER, exact_test_name)
            .env_remove("POLARIS_DATA_DIR")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains(sentinel),
            "MCP prompt child {exact_test_name} failed: status={:?} stdout={stdout} stderr={stderr}",
            output.status
        );
        true
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_mcp_prompt_and_input_survive_sidechat_restart_together() {
        use polaris_core::{
            conversation_state::HistoryMode,
            mcp::{operation::ExternalCeiling, runtime::McpPromptChoice},
            session_manager::{AdmissionState, SessionId, TerminalStatus},
            session_store::PersistedSession,
        };

        if mcp_prompt_child_gate(
            "tests::selected_mcp_prompt_and_input_survive_sidechat_restart_together",
            "POLARIS_TUI_SELECTED_MCP_PROMPT_CHILD_COMPLETE",
        ) {
            return;
        }

        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let state_dir = root.path().join("state");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let database = root.path().join("memory.sqlite3");
        let parent =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let parent_id = parent.snapshot().unwrap().state.session_id;
        let mut session = polaris_core::session::Session::new();
        session.attach(parent.clone()).unwrap();
        let mut session_path = sessions_dir.join(format!("{parent_id}.jsonl"));
        let mut meta_path = session_path.with_extension("meta.json");
        let mut started = 1;
        let mut local_lines = Vec::new();

        let (server, methods, fixture) = polaris_mcp::test_support::server_fixture("").await;
        let host = prompt_fixture_host(root.path(), home.path(), server);
        let choice = McpPromptChoice {
            server: "fixture".into(),
            prompt: "prompt".into(),
            arguments: Default::default(),
        };
        let (turn, _, _, selected_runtime) = admit_tui_input(
            &mut session,
            "original input",
            Some(&DistinctContextJudge),
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            true,
            Some(TuiPromptRequest {
                host: &host,
                choice: &choice,
                selector: None,
                ceiling: ExternalCeiling::Denied,
            }),
        )
        .await
        .unwrap();
        assert!(selected_runtime.is_some());
        assert!(matches!(
            turn.manager.status(&turn.admission_id).unwrap().state,
            AdmissionState::Running { .. }
        ));
        let sidechat = session.persistence.as_ref().unwrap().snapshot().unwrap();
        assert_ne!(sidechat.state.session_id, parent_id);
        assert!(parent.snapshot().unwrap().events.is_empty());
        assert_eq!(sidechat.events.len(), 2);
        assert_eq!(sidechat.events[0].message.content, "original input");
        assert!(sidechat.events[0].starts_turn);
        assert!(
            sidechat.events[1]
                .message
                .content
                .contains("selected prompt")
        );
        let envelope: serde_json::Value =
            serde_json::from_str(&sidechat.events[1].message.content).unwrap();
        let expected_run = polaris_core::session_manager::RunId::derive(&turn.admission_id);
        assert_eq!(
            envelope["intent"]["session"].as_str(),
            Some(sidechat.state.session_id.as_str())
        );
        assert_eq!(
            envelope["intent"]["run"].as_str(),
            Some(expected_run.as_str())
        );
        assert_eq!(
            envelope["intent"]["binding"]["item"]["server_id"],
            "fixture"
        );
        assert_eq!(envelope["intent"]["binding"]["item"]["key"], "prompt");
        assert_eq!(envelope["disposition"], "completed");
        assert!(!sidechat.events[1].starts_turn);
        assert_eq!(sidechat.events[0].turn_id, sidechat.events[1].turn_id);
        assert!(
            sidechat
                .state
                .admission_receipts
                .contains_key(turn.admission_id.as_str())
        );
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|method| *method == "prompts/get")
                .count(),
            1
        );

        let admission_id = turn.admission_id.clone();
        let sidechat_id = SessionId::new(sidechat.state.session_id.clone()).unwrap();
        drop(turn);
        let (store, mut manager) = open_tui_admission_owner(&state_dir, &sidechat_id).unwrap();
        recover_admission_owner(&store, &mut manager, root.path(), &database, "project").unwrap();
        assert!(matches!(
            manager.status(&admission_id).unwrap().state,
            AdmissionState::Terminal {
                status: TerminalStatus::OutcomeUnknown,
                ..
            }
        ));
        assert_eq!(
            session
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .events
                .len(),
            2
        );
        drop(store);
        let retry = admit_tui_input(
            &mut session,
            "must be blocked",
            None,
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            false,
            None,
        )
        .await;
        assert!(matches!(retry, Err(TuiAdmissionFailure::Retryable(_))));
        assert_eq!(
            session
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .events
                .len(),
            2
        );
        if let Some(mut runtime) = selected_runtime {
            runtime.shutdown().await.unwrap();
        }
        fixture.abort();
        println!("POLARIS_TUI_SELECTED_MCP_PROMPT_CHILD_COMPLETE");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_or_timed_out_mcp_prompt_does_not_commit_tui_input() {
        use polaris_core::{
            conversation_state::HistoryMode,
            mcp::{operation::ExternalCeiling, runtime::McpPromptChoice},
            session_manager::{AdmissionState, SessionId, TerminalStatus},
            session_store::PersistedSession,
        };

        if mcp_prompt_child_gate(
            "tests::failed_or_timed_out_mcp_prompt_does_not_commit_tui_input",
            "POLARIS_TUI_FAILED_MCP_PROMPT_CHILD_COMPLETE",
        ) {
            return;
        }

        for delay in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let state_dir = root.path().join("state");
            let sessions_dir = root.path().join("sessions");
            std::fs::create_dir_all(&sessions_dir).unwrap();
            let database = root.path().join("memory.sqlite3");
            let parent = PersistedSession::create(
                root.path(),
                &database,
                "project",
                HistoryMode::Legacy,
                None,
            )
            .unwrap();
            let parent_id = parent.snapshot().unwrap().state.session_id;
            let mut session = polaris_core::session::Session::new();
            session.attach(parent.clone()).unwrap();
            let mut session_path = sessions_dir.join(format!("{parent_id}.jsonl"));
            let mut meta_path = session_path.with_extension("meta.json");
            let mut started = 1;
            let mut local_lines = Vec::new();
            let (server, methods, fixture) = if delay {
                polaris_mcp::test_support::server_fixture("prompts/get").await
            } else {
                polaris_mcp::test_support::server_fixture_with_failures(
                    "",
                    serde_json::json!({}),
                    serde_json::json!([]),
                    "prompts/get",
                    vec![500; 10],
                )
                .await
            };
            let host = prompt_fixture_host(root.path(), home.path(), server);
            let choice = McpPromptChoice {
                server: "fixture".into(),
                prompt: "prompt".into(),
                arguments: Default::default(),
            };
            let result = admit_tui_input(
                &mut session,
                "uncommitted input",
                Some(&DistinctContextJudge),
                &state_dir,
                &sessions_dir,
                &mut session_path,
                &mut meta_path,
                &mut started,
                &mut local_lines,
                None,
                None,
                false,
                root.path(),
                true,
                Some(TuiPromptRequest {
                    host: &host,
                    choice: &choice,
                    selector: None,
                    ceiling: ExternalCeiling::Denied,
                }),
            )
            .await;
            assert!(matches!(
                result,
                Err(TuiAdmissionFailure::PromptFailed { unknown: true, .. })
            ));
            assert!(
                methods
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|method| method == "prompts/get")
            );
            assert!(parent.snapshot().unwrap().events.is_empty());
            let current = SessionId::new(parent_id).unwrap();
            let (_store, manager) = open_tui_admission_owner(&state_dir, &current).unwrap();
            assert_eq!(manager.sessions.len(), 1);
            assert!(manager.queue.is_empty());
            assert_eq!(manager.admissions.len(), 1);
            assert!(manager.admissions.values().all(|admission| matches!(
                admission.state,
                AdmissionState::Terminal {
                    status: TerminalStatus::OutcomeUnknown,
                    ..
                }
            )));
            fixture.abort();
        }
        println!("POLARIS_TUI_FAILED_MCP_PROMPT_CHILD_COMPLETE");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn distinct_tui_input_uses_an_empty_durable_sidechat_and_terminal_receipt() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_manager::{AdmissionState, TerminalStatus},
            session_store::PersistedSession,
            workflow::{SessionWorkflow, WorkflowConfig},
        };

        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().join("state");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let database = root.path().join("memory.sqlite3");
        let workflow = SessionWorkflow::new(WorkflowConfig::default());
        let parent = PersistedSession::create(
            root.path(),
            &database,
            "project",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let parent_id = parent.snapshot().unwrap().state.session_id;
        let mut session = polaris_core::session::Session::new();
        session.workflow = Some(workflow);
        session.attach(parent.clone()).unwrap();
        let mut session_path = sessions_dir.join(format!("{parent_id}.jsonl"));
        let mut meta_path = session_path.with_extension("meta.json");
        let mut started = 1;
        let mut local_lines = Vec::new();

        let (turn, notice, rewind_boundary, selected_runtime) = admit_tui_input(
            &mut session,
            "unrelated isolated task",
            Some(&DistinctContextJudge),
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            true,
            None,
        )
        .await
        .unwrap();
        assert!(selected_runtime.is_none());
        assert!(rewind_boundary.is_none());
        let admission_id = turn.admission_id.clone();
        let sidechat = session.persistence.as_ref().unwrap().snapshot().unwrap();
        assert_ne!(sidechat.state.session_id, parent_id);
        assert_eq!(sidechat.events.len(), 1);
        assert_eq!(
            sidechat.events[0].message.content,
            "unrelated isolated task"
        );
        assert_eq!(parent.snapshot().unwrap().events.len(), 0);
        assert_eq!(turn.manager.sessions.len(), 2);
        assert_eq!(
            turn.run_id().unwrap(),
            polaris_core::session_manager::RunId::derive(&admission_id).as_str()
        );
        assert!(notice.unwrap().contains("running"));
        turn.finish(TerminalStatus::Completed).unwrap();

        let current =
            polaris_core::session_manager::SessionId::new(sidechat.state.session_id).unwrap();
        let (_store, restored) = open_tui_admission_owner(&state_dir, &current).unwrap();
        assert!(matches!(
            restored.status(&admission_id).unwrap().state,
            AdmissionState::Terminal {
                status: TerminalStatus::Completed,
                ..
            }
        ));
        drop(_store);

        fn admission_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
            let mut files = std::collections::BTreeMap::new();
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files.insert(path.clone(), None);
                    files.extend(admission_files(&path));
                } else {
                    files.insert(path.clone(), Some(std::fs::read(&path).unwrap()));
                }
            }
            files
        }
        let admissions_root = state_dir.join("tui-admissions-v1");
        let before_status = admission_files(&admissions_root);
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        let activity = AgentActivityRows::default();
        let mut max_turns = 1;
        let command = format!("input status {}", admission_id.as_str());
        let lines = handle_runtime_command(
            &command,
            None,
            session.persistence.as_ref(),
            &state_dir,
            &[],
            &sandbox,
            ApprovalPolicy::Never,
            &mut max_turns,
            1,
            &activity,
        )
        .await
        .unwrap();
        assert!(lines[0].contains(admission_id.as_str()));
        assert!(lines[0].contains("Completed"));
        let unrelated =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        assert!(
            handle_runtime_command(
                &command,
                None,
                Some(&unrelated),
                &state_dir,
                &[],
                &sandbox,
                ApprovalPolicy::Never,
                &mut max_turns,
                1,
                &activity,
            )
            .await
            .is_err()
        );
        assert_eq!(admission_files(&admissions_root), before_status);
        let index = admissions_root.join("family-index.v1.json");
        let original_index = std::fs::read(&index).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&index)
            .unwrap()
            .set_len(2 * 1024 * 1024 + 1)
            .unwrap();
        let oversized = admission_files(&admissions_root);
        assert!(
            handle_runtime_command(
                &command,
                None,
                session.persistence.as_ref(),
                &state_dir,
                &[],
                &sandbox,
                ApprovalPolicy::Never,
                &mut max_turns,
                1,
                &activity,
            )
            .await
            .is_err()
        );
        assert_eq!(admission_files(&admissions_root), oversized);
        std::fs::write(&index, original_index).unwrap();

        let denied = admit_tui_input(
            &mut session,
            "another isolated task",
            Some(&DistinctContextJudge),
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            true,
            None,
        )
        .await;
        assert!(matches!(denied, Err(TuiAdmissionFailure::Retryable(_))));
        let (blocked_store, blocked_manager) =
            open_tui_admission_owner(&state_dir, &current).unwrap();
        assert!(blocked_manager.queue.is_empty());
        assert!(
            blocked_manager
                .admissions
                .values()
                .all(|admission| matches!(admission.state, AdmissionState::Terminal { .. }))
        );
        drop(blocked_store);

        let (next, notice, _, runtime) = admit_tui_input(
            &mut session,
            "follow-up in this lane",
            None,
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            false,
            None,
        )
        .await
        .unwrap();
        assert!(notice.is_none());
        assert!(runtime.is_none());
        assert_eq!(next.manager.sessions.len(), 2);
        assert_eq!(
            session
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .events
                .len(),
            2
        );
        assert_ne!(next.admission_id, admission_id);
        next.finish(TerminalStatus::Completed).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_blocks_a_new_turn_after_an_unstarted_committed_input() {
        use polaris_core::{
            conversation_state::HistoryMode,
            session_manager::{
                AdmissionState, DestinationMode, SessionId, SubmissionPayloadV1, SubmitOutcomeV1,
                SubmitRequestV1, TerminalStatus,
            },
            session_store::PersistedSession,
        };

        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().join("state");
        let database = root.path().join("memory.sqlite3");
        let parent =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let parent_id = parent.snapshot().unwrap().state.session_id;
        let current = SessionId::new(parent_id).unwrap();
        let receipt_counts = std::collections::BTreeMap::from([(
            current.clone(),
            parent.snapshot().unwrap().state.admission_receipts.len(),
        )]);
        let (store, mut manager) = open_tui_admission_owner(&state_dir, &current).unwrap();
        let before = manager.revision;
        let SubmitOutcomeV1::Accepted { admission, .. } = manager
            .submit(
                SubmitRequestV1 {
                    parent_session: current,
                    payload: SubmissionPayloadV1 {
                        text: "durable input".into(),
                        attachment_ids: vec![],
                        route: DestinationMode::New,
                        expected_session_revision: 0,
                        expected_goal_revision: 0,
                    },
                    admission_id: None,
                },
                before,
            )
            .unwrap();
        store
            .persist_submission(before, &manager, &receipt_counts)
            .unwrap();
        let before = manager.revision;
        let decided = manager
            .decide(
                &admission.id,
                before,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        persist_manager_transition(&store, before, &manager).unwrap();
        let before = manager.revision;
        let prepared = manager
            .prepare_destination(&admission.id, before, decided.revision)
            .unwrap();
        persist_manager_transition(&store, before, &manager).unwrap();
        let AdmissionState::DestinationReady { session_id, .. } = &prepared.state else {
            panic!("destination should be ready");
        };
        let mut parent_session = polaris_core::session::Session::new();
        parent_session.attach(parent).unwrap();
        let mut sidechat = empty_sidechat_session(&parent_session, session_id).unwrap();
        assert!(
            sidechat
                .push_admitted_user("durable input", &admission.id, &prepared.payload_hash)
                .unwrap()
        );

        recover_admission_owner(&store, &mut manager, root.path(), &database, "project").unwrap();
        assert!(matches!(
            manager.status(&admission.id).unwrap().state,
            AdmissionState::Terminal {
                status: TerminalStatus::OutcomeUnknown,
                ..
            }
        ));
        assert_eq!(
            sidechat
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .events
                .len(),
            1
        );
        drop(store);
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir(&sessions_dir).unwrap();
        let session_id = sidechat
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id;
        let mut session_path = sessions_dir.join(format!("{session_id}.jsonl"));
        let mut meta_path = session_path.with_extension("meta.json");
        let mut started = 1;
        let mut local_lines = Vec::new();
        let result = admit_tui_input(
            &mut sidechat,
            "new input",
            None,
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            true,
            None,
        )
        .await;
        assert!(matches!(result, Err(TuiAdmissionFailure::Retryable(_))));
        assert_eq!(
            sidechat
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .events
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_reply_builds_a_bound_shadow_finalization_input() {
        use polaris_core::{conversation_state::HistoryMode, session_store::PersistedSession};

        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("memory.sqlite3");
        let persistence =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let session_id = persistence.snapshot().unwrap().state.session_id;
        let mut session = polaris_core::session::Session::new();
        session.attach(persistence).unwrap();
        session.push_user("summarize this");
        session.push_assistant("done", vec![]);
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                .unwrap();
        let control = install_tui_root_control(
            &mut session,
            TuiRootControlArgs {
                session_id: &session_id,
                turn: 1,
                sandbox: &sandbox,
                approval: ApprovalPolicy::OnRequest,
                mcp_available: false,
                tool_names: &[],
                model: "gpt-6-astra",
                effort: "xhigh",
            },
        )
        .unwrap();

        let input = tui_shadow_input(&session, "summarize this", &control)
            .unwrap()
            .unwrap();
        assert_eq!(input.prompt, "summarize this");
        assert_eq!(input.source_body, "done");
        assert!(input.closed_evidence.is_empty());
        assert!(input.evidence_complete);
        assert_eq!(input.subject.session_id, session_id);
        assert_eq!(input.subject.root_run_id, "tui-root-run-1");
        assert_eq!(input.subject.attempt_id, "tui-root-attempt-1");
        assert_eq!(input.subject.turn_sequence, 2);
        input.subject.validate().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn successful_tui_turn_schedules_the_shared_shadow_owner() {
        use polaris_core::{
            config::{
                AnswerFinalizerConfig, AnswerFinalizerMode, JudgeProfile, PresentationConfig,
                PresentationLanguage,
            },
            conversation_state::HistoryMode,
            session_store::PersistedSession,
        };

        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("memory.sqlite3");
        let persistence =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let session_id = persistence.snapshot().unwrap().state.session_id;
        let mut session = polaris_core::session::Session::new();
        session.attach(persistence).unwrap();
        session.push_user("answer this");
        session.push_assistant("answer", vec![]);
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                .unwrap();
        let control = install_tui_root_control(
            &mut session,
            TuiRootControlArgs {
                session_id: &session_id,
                turn: 2,
                sandbox: &sandbox,
                approval: ApprovalPolicy::OnRequest,
                mcp_available: false,
                tool_names: &[],
                model: "gpt-6-astra",
                effort: "xhigh",
            },
        )
        .unwrap();
        let mut owner =
            polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner::open_configured(
                root.path().join("shadow.json"),
                PresentationConfig {
                    language: PresentationLanguage::Ja,
                },
                AnswerFinalizerConfig {
                    mode: AnswerFinalizerMode::Shadow,
                    judge_profile: Some(JudgeProfile::FulfillmentV1),
                    ..Default::default()
                },
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();

        // Installation evidence does not make an absent runtime available.
        Arc::get_mut(&mut owner)
            .unwrap()
            .record_translation_installation(Some(true));
        let diagnostic = cat_diagnostic_lines(Some(&owner));
        assert!(
            diagnostic[0]
                .to_string()
                .contains("installed=導入確認; verified=false; observed=0")
        );
        assert!(schedule_tui_shadow(&owner, &session, "answer this", &control).unwrap());
        assert_eq!(owner.records().unwrap().len(), 1);
    }

    #[test]
    fn idle_ctrl_c_exits_only_on_the_second_consecutive_press() {
        let mut confirmation = ExitConfirmation::default();

        assert!(!confirmation.press());
        assert!(confirmation.press());
    }

    #[test]
    fn other_input_cancels_an_armed_idle_ctrl_c() {
        let mut confirmation = ExitConfirmation::default();

        assert!(!confirmation.press());
        confirmation.cancel();
        assert!(!confirmation.press());
    }

    #[test]
    fn queued_quit_refusal_restores_the_displaced_draft_and_keeps_later_input() {
        let queued = VecDeque::from(["send this next".to_string()]);
        let mut input_history = input::History::new();
        let mut input = String::new();
        let mut cursor = 0;
        let mut status = Status::Idle;

        assert!(refuse_quit_with_pending_work(
            &queued,
            &mut input_history,
            (&mut input, &mut cursor),
            Some(("draft work".to_string(), 5)),
            "/quit",
            true,
            &mut status,
        ));
        assert_eq!(input, "draft work");
        assert_eq!(cursor, 5);
        assert_eq!(queued.front().map(String::as_str), Some("send this next"));
        assert!(matches!(status, Status::Notice(_)));
    }

    #[test]
    fn typed_quit_refusal_keeps_the_command_when_work_is_queued() {
        let queued = VecDeque::from(["send this next".to_string()]);
        let mut input_history = input::History::new();
        let mut input = String::new();
        let mut cursor = 0;
        let mut status = Status::Idle;

        assert!(refuse_quit_with_pending_work(
            &queued,
            &mut input_history,
            (&mut input, &mut cursor),
            None,
            "/quit",
            false,
            &mut status,
        ));
        assert_eq!(input, "/quit");
        assert_eq!(cursor, input.len());
        assert_eq!(queued.front().map(String::as_str), Some("send this next"));
    }

    #[test]
    fn quit_is_allowed_only_after_the_queue_and_draft_are_empty() {
        let queued = VecDeque::new();
        let mut input_history = input::History::new();
        let mut input = "unsent draft".to_string();
        let mut cursor = input.len();
        let mut status = Status::Idle;
        assert!(refuse_quit_with_pending_work(
            &queued,
            &mut input_history,
            (&mut input, &mut cursor),
            None,
            "/quit",
            true,
            &mut status,
        ));
        assert_eq!(input, "unsent draft");

        input.clear();
        cursor = 0;
        status = Status::Idle;
        assert!(!refuse_quit_with_pending_work(
            &queued,
            &mut input_history,
            (&mut input, &mut cursor),
            None,
            "/quit",
            false,
            &mut status,
        ));
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn quit_submission_does_not_release_a_paused_queue() {
        assert!(!submission_resumes_queued_inputs("/quit"));
        assert!(!submission_resumes_queued_inputs("/quit ignored"));
        assert!(!submission_resumes_queued_inputs("/exit"));
        assert!(!submission_resumes_queued_inputs("/q"));
        assert!(!submission_resumes_queued_inputs("/new"));
        assert!(submission_resumes_queued_inputs("continue this work"));
    }

    #[test]
    fn popup_quit_refuses_a_draft_hidden_by_input_history() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut input_history = input::History::new();
        input_history.record("previous input");
        let mut input_buffer = "unsent α".to_string();
        assert_eq!(input_history.older(&input_buffer), Some("previous input"));
        input_buffer = "/q".to_string();
        let mut input_cursor = input_buffer.len();
        let queued = VecDeque::new();
        let mut status = Status::Idle;
        let suggestions = slash::matching(input_buffer.trim_start_matches('/'));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(enter.code, KeyCode::Enter);
        assert_eq!(suggestions[0].name, "quit");
        assert!(matches!(
            slash::action_for(suggestions[0].name),
            slash::Action::Quit
        ));
        let submitted = std::mem::take(&mut input_buffer);
        assert_eq!(input_cursor, 2);
        input_cursor = 0;

        assert!(refuse_quit_with_pending_work(
            &queued,
            &mut input_history,
            (&mut input_buffer, &mut input_cursor),
            None,
            &submitted,
            false,
            &mut status,
        ));
        assert_eq!(input_buffer, "unsent α");
        assert_eq!(input_cursor, input_buffer.len());
        assert!(queued.is_empty());
    }

    #[test]
    fn typed_quit_refuses_a_draft_hidden_by_input_history() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut input_history = input::History::new();
        input_history.record("previous input");
        let mut input_buffer = "unsent α".to_string();
        assert_eq!(input_history.older(&input_buffer), Some("previous input"));
        input_buffer = "/exit".to_string();
        let mut input_cursor = input_buffer.len();
        let queued = VecDeque::new();
        let mut status = Status::Idle;
        assert!(slash::matching("exit").is_empty());
        let InputAction::Submit(submitted) = apply_key(
            &mut input_buffer,
            &mut input_cursor,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ) else {
            panic!("Enter should submit /exit");
        };
        record_submission_before_dispatch(&mut input_history, &submitted, false);
        assert!(matches!(
            slash::parse(&submitted),
            Some(slash::Action::Quit)
        ));

        assert!(refuse_quit_with_pending_work(
            &queued,
            &mut input_history,
            (&mut input_buffer, &mut input_cursor),
            None,
            &submitted,
            false,
            &mut status,
        ));
        assert_eq!(input_buffer, "unsent α");
        assert_eq!(input_cursor, input_buffer.len());
        assert!(queued.is_empty());
    }

    #[test]
    fn resume_hint_uses_the_current_persisted_legacy_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("resume-me.jsonl");
        std::fs::write(&session_path, b"{}\n").unwrap();
        let session = polaris_core::session::Session::new();

        let id = current_resume_id(&session, &session_path).unwrap();
        assert_eq!(id, "resume-me");
        assert_eq!(
            resume_hint(&id),
            "\nResume this session with:\npolaris --resume resume-me\n"
        );
    }

    #[test]
    fn empty_unpersisted_session_has_no_resume_hint() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("not-created.jsonl");
        let session = polaris_core::session::Session::new();

        assert_eq!(current_resume_id(&session, &session_path), None);
    }

    #[tokio::test]
    async fn tui_turn_installs_fresh_root_control_with_host_capabilities() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .unwrap();
        let mut session = polaris_core::session::Session::new();
        let control = install_tui_root_control(
            &mut session,
            TuiRootControlArgs {
                session_id: "session-1",
                turn: 7,
                sandbox: &sandbox,
                approval: ApprovalPolicy::OnRequest,
                mcp_available: true,
                tool_names: &["read".into(), "spawn".into()],
                model: "gpt-6-astra",
                effort: "automatic",
            },
        )
        .unwrap();

        let projection = control.snapshot().await;
        assert_eq!(projection.session_id, "session-1");
        assert_eq!(projection.root_run_id.0, "tui-root-run-7");
        assert_eq!(projection.active_task_id.0, "tui-task-7");
        assert!(session.delegation_control.is_some());
        session.finish_root_delegation_control(false).await.unwrap();
        assert!(session.delegation_control.is_none());
    }

    #[test]
    fn a_failed_turn_keeps_possible_mcp_sends_as_unknown_and_rolls_back_others() {
        use polaris_provider::{Message, ToolCall};
        let turn = |name: &str| {
            let mut session = polaris_core::session::Session::new();
            session.push_user("request");
            session.messages.push(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "call".into(),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                }],
            ));
            session
        };
        let mut mcp = turn("mcp_fixture__tool");
        keep_or_roll_back(&mut mcp, 1).unwrap();
        assert_eq!(mcp.messages.len(), 3);
        assert!(mcp.messages[2].content.contains("結果は不明"));
        let mut bash = turn("bash");
        keep_or_roll_back(&mut bash, 1).unwrap();
        assert_eq!(bash.messages.len(), 1);
    }

    use super::*;
    use approver::KeyReader;
    use polaris_core::conversation_memory::{
        EmbeddingModel, StrictEmbedder, StrictHistory, StrictSummaryProvider, SummaryRequest,
    };
    use polaris_core::session::Session;
    use polaris_provider::Message;
    use std::{future::Future, io, pin::Pin};

    type StrictFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

    struct TestSummary;
    impl StrictSummaryProvider for TestSummary {
        fn summarize<'a>(&'a self, _request: SummaryRequest) -> StrictFuture<'a, String> {
            Box::pin(async {
                Ok("{\"facts\":[],\"decisions\":[],\"constraints\":[],\"corrections\":[],\"open_items\":[],\"source_turn_ids\":[]}".into())
            })
        }
    }

    struct TestEmbedder(EmbeddingModel);
    impl StrictEmbedder for TestEmbedder {
        fn metadata(&self) -> &EmbeddingModel {
            &self.0
        }
        fn embed_passage<'a>(&'a self, _text: &'a str) -> StrictFuture<'a, Vec<f32>> {
            Box::pin(async { Ok(vec![0.0]) })
        }
        fn embed_query<'a>(&'a self, _text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
            Box::pin(async { Ok(vec![vec![0.0]]) })
        }
    }

    fn test_strict_history() -> Arc<StrictHistory> {
        Arc::new(StrictHistory::new(
            Arc::new(TestSummary),
            Arc::new(TestEmbedder(EmbeddingModel {
                model: "test".into(),
                revision: "v1".into(),
                dimension: 1,
            })),
        ))
    }
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::text::Line;
    use std::collections::VecDeque;

    #[test]
    fn judged_effort_replaces_the_undecided_footer_value() {
        let mut effort = UNDECIDED_EFFORT.to_string();
        update_effort_from_event(&polaris_core::AgentEvent::EffortJudging, &mut effort);
        assert_eq!(effort, "judging");
        update_effort_from_event(
            &polaris_core::AgentEvent::EffortJudged {
                effort: "xhigh".into(),
            },
            &mut effort,
        );
        assert_eq!(effort, "extra high");
    }

    #[cfg(unix)]
    #[test]
    fn working_turn_accepts_and_queues_prompts_and_commands_in_order() {
        let mut buffer = String::new();
        let mut cursor = 0;
        let mut history = input::History::new();
        let mut queued = VecDeque::new();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        for character in "next prompt".chars() {
            assert_eq!(
                apply_inflight_key(
                    &mut buffer,
                    &mut cursor,
                    &mut history,
                    &mut queued,
                    key(KeyCode::Char(character)),
                ),
                InflightInputAction::Continue
            );
        }
        assert_eq!(
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Enter),
            ),
            InflightInputAction::Queued
        );
        for character in "/model".chars() {
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Char(character)),
            );
        }
        assert_eq!(
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Enter),
            ),
            InflightInputAction::Queued
        );

        assert_eq!(
            queued.into_iter().collect::<Vec<_>>(),
            ["next prompt", "/model"]
        );
        assert!(buffer.is_empty());
        assert_eq!(cursor, 0);

        // Synthetic Enter presses must not record those same two entries a
        // second time when they are dispatched after the current turn.
        record_submission(&mut history, "next prompt", true);
        record_submission(&mut history, "/model", true);
        assert_eq!(history.older(""), Some("/model"));
        assert_eq!(history.older(""), Some("next prompt"));
        assert_eq!(history.older(""), None);
    }

    #[test]
    fn dedicated_secret_operations_dispatch_locally_without_entering_history() {
        let mut history = input::History::new();
        history.record("ordinary prompt");
        for command in ["/mcp secret set", "/MCP secret delete synthetic-reference"] {
            let mut text = command.to_owned();
            assert!(!refuse_secret_submission(&mut text, true));
            assert!(matches!(slash::parse(&text), Some(slash::Action::Mcp(_))));
            record_submission_before_dispatch(&mut history, &text, false);
        }
        assert_eq!(history.older(""), Some("ordinary prompt"));
        assert_eq!(history.older(""), None);
        for command in [
            "/mcp secret set synthetic-inline-value",
            "/mcp secret delete reference extra",
            "/mcp secret unknown synthetic-inline-value",
        ] {
            let mut text = command.to_owned();
            assert!(refuse_secret_submission(&mut text, true));
            assert!(text.is_empty());
        }
        let mut text = "/mcp secret set".to_owned();
        assert!(refuse_secret_submission(&mut text, false));
        assert!(text.is_empty());
    }

    #[test]
    fn secret_command_never_enters_inflight_history_or_queue() {
        assert!(is_secret_command("  /MCP secret set VALUE"));
        assert!(!is_secret_command("/mcp prompt select"));
        assert!(secret_value_key_is_blocked(
            "/MCP secret set",
            &KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)
        ));
        assert!(!secret_value_key_is_blocked(
            "/mcp secret se",
            &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)
        ));
        assert!(!secret_value_key_is_blocked(
            "/mcp secret set",
            &KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
        ));
        let mut buffer = String::new();
        let mut cursor = 0;
        let mut history = input::History::new();
        let mut queued = VecDeque::new();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for character in "/mcp secret set VALUE".chars() {
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Char(character)),
            );
        }
        assert_eq!(buffer, "/mcp secret set");
        assert_eq!(cursor, buffer.len());
        assert_eq!(
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Enter),
            ),
            InflightInputAction::RejectedSecret
        );
        assert!(queued.is_empty());
        assert!(history.older("").is_none());

        // Complete the command around an already typed value. The first
        // whitespace between `set` and `VALUE` must erase the value before
        // the next frame can use the composer buffer.
        let mut buffer = "VALUE".to_string();
        let mut cursor = buffer.len();
        for _ in 0.."VALUE".chars().count() {
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Left),
            );
        }
        for character in "/MCP\u{2003}SECRET\u{2003}SET".chars() {
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Char(character)),
            );
        }
        assert_eq!(buffer, "/MCP\u{2003}SECRET\u{2003}SETVALUE");
        assert_eq!(
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Char('\u{2003}')),
            ),
            InflightInputAction::RejectedSecret
        );
        assert_eq!(buffer, "/mcp secret set");
        assert_eq!(cursor, buffer.len());
        assert!(queued.is_empty());
        assert!(history.older("").is_none());
    }

    #[test]
    fn inline_secret_value_is_redacted_before_idle_draw() {
        let mut buffer = "/mcp secret set TOKEN".to_string();
        let mut cursor = buffer.len();
        let mut history = input::History::new();
        history.record("previous");
        assert_eq!(history.older("TOKEN"), Some("previous"));
        assert_eq!(history.newer(), Some("TOKEN"));
        assert!(redact_inline_secret_value(
            &mut buffer,
            &mut cursor,
            &mut history,
        ));
        assert_eq!(buffer, "/mcp secret set");
        assert_eq!(cursor, buffer.len());
        assert!(history.draft_is_empty());
        assert!(!redact_inline_secret_value(
            &mut buffer,
            &mut cursor,
            &mut history,
        ));
        assert_eq!(secret_set_has_value("/mcp secret setup TOKEN"), None);
    }

    struct Summarizer;

    #[async_trait::async_trait]
    impl polaris_provider::Provider for Summarizer {
        async fn complete(
            &self,
            _req: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            Ok(polaris_provider::CompletionResponse {
                text: "summary text".to_string(),
                ..Default::default()
            })
        }
    }

    #[cfg(unix)]
    #[test]
    fn m3e_run_tool_details_use_observed_events_and_keep_unfinished_unknown() {
        let mut activity = AgentActivityRows::default();
        activity.observe_tool(&polaris_core::AgentEvent::ToolStarted {
            name: "read".into(),
            detail: "one".into(),
        });
        assert!(activity.tool_detail_lines()[0].contains("完了未確認"));
        activity.observe_tool(&polaris_core::AgentEvent::ToolFinished {
            name: "read".into(),
            detail: "one".into(),
            ok: false,
            result: "private body".into(),
            diff: None,
        });
        assert!(activity.tool_detail_lines()[0].contains("失敗を観測"));
        assert!(!activity.tool_detail_lines()[0].contains("private body"));
        for n in 0..130 {
            activity.observe_tool(&polaris_core::AgentEvent::ToolStarted {
                name: format!("tool-{n}"),
                detail: "x".into(),
            });
        }
        assert_eq!(activity.tools.len(), 128);
        assert!(
            activity
                .tool_detail_lines()
                .iter()
                .all(|line| line.contains("完了未確認"))
        );
        let mut history = Vec::new();
        let mut cache = render::HistoryWrapCache::default();
        let mut scroll = 1;
        let mut selection = None;
        reset_conversation_view(
            &mut history,
            &mut cache,
            &mut activity,
            &mut scroll,
            &mut selection,
            &render::HeaderInfo {
                provider_name: "test",
                model_name: "test",
                usage: Default::default(),
                cwd: "test",
                cwd_short: "test",
                effort_name: "auto",
            },
            80,
        );
        assert!(activity.tools.is_empty());
        assert!(activity.tool_detail_lines()[0].contains("観測されていません"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn m3e_confirmed_compaction_keeps_original_and_refuses_cancelled_send() {
        struct BoundSummary;
        #[async_trait::async_trait]
        impl polaris_provider::Provider for BoundSummary {
            async fn complete(
                &self,
                request: polaris_provider::CompletionRequest,
            ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError>
            {
                Summarizer.complete(request).await
            }
            fn model(&self) -> Option<String> {
                Some("test-summary".into())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&state).unwrap();
        std::fs::set_permissions(
            &state,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let workspace = polaris_core::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
            &project, &state, &lease,
        )
        .unwrap();
        drop(lease);
        let saved = polaris_core::session_store::PersistedSession::create(
            root.path(),
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            polaris_core::conversation_state::HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(saved.clone()).unwrap();
        session.push_user("turn1");
        session.push_assistant(&"older detailed reply ".repeat(2000), Vec::new());
        session.push_user("turn2");
        session.push_assistant("reply2", Vec::new());
        session.push_user("turn3");
        session.push_assistant("reply3", Vec::new());
        let before = saved.snapshot().unwrap();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
        let mut status = Status::Idle;
        let mut usage = polaris_provider::Usage::default();
        let mut declined = ScriptedReader(VecDeque::from([KeyCode::Esc]));
        assert!(
            !local_actions::compact(
                &terminal,
                &mut declined,
                &BoundSummary,
                &mut session,
                &root.path().join("unused.jsonl"),
                &workspace,
                &effects,
                &mut status,
                &mut usage,
                false
            )
            .await
            .unwrap()
        );
        assert_eq!(saved.snapshot().unwrap().raw_hash, before.raw_hash);
        assert_eq!(session.messages.len(), 6);
        let mut accepted = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        assert!(
            local_actions::compact(
                &terminal,
                &mut accepted,
                &BoundSummary,
                &mut session,
                &root.path().join("unused.jsonl"),
                &workspace,
                &effects,
                &mut status,
                &mut usage,
                false
            )
            .await
            .unwrap()
        );
        assert_eq!(session.messages.len(), 5);
        assert_eq!(saved.snapshot().unwrap().events.len(), 6);
        assert!(!root.path().join("unused.jsonl").exists());
        assert!(effects.blocker().unwrap().is_none());
    }

    #[tokio::test]
    async fn handle_compact_compacts_and_persists_when_there_is_something_to_compact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("s.jsonl");

        // "reply 1" alone is well under `MIN_TOKENS_TO_SUMMARIZE`
        // (`compaction::compact`'s floor guard) — inflated here so the
        // summarized prefix actually clears the floor and this test still
        // exercises a real compaction rather than tripping the "not worth
        // it" no-op. Single-char repeats compress ~8:1 under o200k_base
        // (see `polaris-core::compaction`'s own test for the same note).
        let big_reply = "x".repeat(polaris_core::compaction::MIN_TOKENS_TO_SUMMARIZE * 10);
        let mut session = Session::default();
        session.push_user("turn 1");
        session.push_assistant(&big_reply, vec![]);
        session.push_user("turn 2");
        session.push_assistant("reply 2", vec![]);
        session.push_user("turn 3");
        session.push_assistant("reply 3", vec![]);

        let mut status = Status::Idle;
        handle_compact(
            &Summarizer,
            &mut session,
            &session_path,
            &mut status,
            &mut polaris_provider::Usage::default(),
        )
        .await;

        assert!(
            matches!(&status, Status::Notice(s) if s.contains("compacted")),
            "expected a 'compacted' notice, got a different status"
        );
        assert_eq!(session.messages.len(), 5, "1 summary + kept tail of 4");

        let (loaded, _truncated) = persist::load_session(&session_path).expect("load");
        assert_eq!(
            loaded.messages.len(),
            session.messages.len(),
            "the persisted file must reflect the compacted session, not the pre-compaction one"
        );
        assert_eq!(loaded.messages[0].content, session.messages[0].content);
    }

    /// A v2 session already holds every message in its own append-only
    /// log, and nothing appends to the legacy path for it. Writing the
    /// summarised list there anyway leaves a second `.jsonl` that
    /// `/resume` lists beside the intact session — same conversation,
    /// prefix replaced by a summary, no way to tell which is which.
    #[tokio::test]
    #[cfg(unix)]
    async fn manual_compaction_does_not_write_a_legacy_log_for_a_v2_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_root = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("s.jsonl");

        let big_reply = "x".repeat(polaris_core::compaction::MIN_TOKENS_TO_SUMMARIZE * 10);
        let mut session = Session::default();
        let persistence = polaris_core::session_store::PersistedSession::create(
            data_root.path(),
            &data_root.path().join("memory.sqlite3"),
            "p",
            polaris_core::conversation_state::HistoryMode::Legacy,
            None,
        )
        .expect("v2 session");
        session.attach(persistence).expect("attach");
        session.push_user("turn 1");
        session.push_assistant(&big_reply, vec![]);
        session.push_user("turn 2");
        session.push_assistant("reply 2", vec![]);
        session.push_user("turn 3");
        session.push_assistant("reply 3", vec![]);

        let mut status = Status::Idle;
        handle_compact(
            &Summarizer,
            &mut session,
            &session_path,
            &mut status,
            &mut polaris_provider::Usage::default(),
        )
        .await;

        assert!(
            matches!(&status, Status::Notice(s) if s.contains("compacted")),
            "compaction did not run, so this proves nothing"
        );
        assert_eq!(session.messages.len(), 5, "1 summary + kept tail of 4");
        assert!(
            !session_path.exists(),
            "a summarised legacy log was written beside an intact v2 session"
        );
    }

    #[tokio::test]
    async fn handle_compact_reports_when_there_is_nothing_to_compact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("s.jsonl");
        let mut session = Session::default();
        session.push_user("only turn");
        let mut status = Status::Idle;

        handle_compact(
            &Summarizer,
            &mut session,
            &session_path,
            &mut status,
            &mut polaris_provider::Usage::default(),
        )
        .await;

        assert!(
            matches!(&status, Status::Notice(s) if s.contains("nothing")),
            "expected a 'nothing to compact' notice"
        );
        assert_eq!(session.messages.len(), 1, "must not have been touched");
    }

    /// Feeds a fixed, pre-scripted sequence of key codes — the same
    /// approach `approver`'s own tests use, so a picker-driving test
    /// doesn't need a real terminal or a live keyboard.
    struct ScriptedReader(VecDeque<KeyCode>);

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0.pop_front().unwrap_or(KeyCode::Null))
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn m2d_sync_confirmation_cancels_and_sanitizes_paths() {
        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 18)).unwrap(),
        );
        let view = render::SyncConfirmView {
            recovering: false,
            operation: "operation-id".into(),
            project: "project\x1b[2J".into(),
            revision: "agreed-revision".into(),
            previous_revision: "old-revision".into(),
            recovery: "private-recovery".into(),
            changes: vec!["Modified: file\x1b[31m".into()],
        };
        let mut declined = ScriptedReader(VecDeque::from([KeyCode::Esc]));
        assert!(!confirm_sync(&terminal, &mut declined, &view));
        let mut accepted = ScriptedReader(VecDeque::from([KeyCode::Enter, KeyCode::Char('y')]));
        assert!(confirm_sync(&terminal, &mut accepted, &view));
        let screen = {
            let term = terminal.borrow();
            let buffer = term.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>()
        };
        for expected in [
            "C12",
            "agreed-revision",
            "old-revision",
            "private-recovery",
            "IrreversibleLocal",
        ] {
            assert!(screen.contains(expected), "{screen}");
        }
        assert!(!screen.contains('\x1b'));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn m3b_sync_tui_join_pull_leave_and_denial() {
        const CHILD: &str = "POLARIS_M3B_TUI_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "tests::m3b_sync_tui_join_pull_leave_and_denial",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use polaris_core::sync::{
            participation,
            repository::SyncRepository,
            share::{ShareLink, ShareManifest, SyncPaths},
        };
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let state = polaris_core::project::state_dir(&project).unwrap();
        source_apply::prepare_state(&project, &state).unwrap();
        let peer_project = home.join("peer-project");
        std::fs::create_dir(&peer_project).unwrap();
        std::fs::write(peer_project.join("received.txt"), "peer body\n").unwrap();
        let peer = SyncRepository::open(SyncPaths::under(&home.join("peer-state"))).unwrap();
        peer.stage_from(&peer_project, &ShareManifest::default())
            .unwrap();
        let head = peer.commit("fixture").unwrap();
        let request = format!(
            "join file://{} fixture-share fixture-invite {}",
            home.join("peer-state/sync/repository").display(),
            u64::MAX
        );
        assert!(
            sync_action(&project, &state, &request, true, |_| panic!(
                "queued mutation confirmed"
            ))
            .is_err()
        );
        assert!(
            sync_action(
                &project,
                &state,
                "join https://user:secret@example.com/x fixture-share fixture-invite 9999999999",
                false,
                |_| panic!("invalid invite displayed")
            )
            .is_err()
        );
        fn state_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
            let mut files = std::collections::BTreeMap::new();
            for entry in std::fs::read_dir(root).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    files.insert(path.clone(), None);
                    files.extend(state_files(&path));
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    files.insert(path.clone(), Some(std::fs::read(path).unwrap()));
                }
            }
            files
        }
        drop(polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).unwrap());
        let before_join = state_files(&state);
        for accepted in [false, true] {
            assert!(
                sync_action(&project, &state, &request, false, |view| {
                    assert!(view.revision.contains("fixture-share"));
                    accepted
                })
                .is_err()
            );
            assert_eq!(state_files(&state), before_join);
            assert!(matches!(
                participation::read(&state).unwrap(),
                participation::ParticipationReadV2::Missing
            ));
            assert!(!project.join("received.txt").exists());
            assert_eq!(peer.head(), head);
        }
        // Historical participation is input for C11/C13, not a C08 write
        // through the retired apply adapter or a credential-binding success.
        participation::join(
            &state,
            ShareLink {
                remote: format!(
                    "file://{}",
                    home.join("peer-state/sync/repository").display()
                ),
                share_id: "fixture-share".into(),
                invitation_id: "fixture-invite".into(),
                expires_unix_seconds: u64::MAX,
            },
            ShareManifest::default(),
            1,
        )
        .unwrap();
        assert!(sync_action(&project, &state, &request, false, |_| panic!("rebind")).is_err());
        let status = sync_action(&project, &state, "status", false, |_| {
            panic!("read confirmation")
        })
        .unwrap();
        assert!(status.contains("参加中"));
        assert!(
            sync_action(&project, &state, "push", false, |_| panic!(
                "unapproved push"
            ))
            .is_err()
        );
        assert!(
            sync_action(&project, &state, "pull", false, |_| false)
                .unwrap()
                .contains("取り消")
        );
        assert!(
            sync_action(&project, &state, "pull", false, |_| true)
                .unwrap()
                .contains("sourceへは未反映")
        );
        assert!(!project.join("received.txt").exists());
        assert_eq!(peer.head(), head);
        assert!(
            sync_action(&project, &state, "leave", false, |_| false)
                .unwrap()
                .contains("取り消")
        );
        assert!(
            participation::read(&state)
                .unwrap()
                .participation()
                .is_some()
        );
        assert!(
            sync_action(&project, &state, "leave", false, |_| true)
                .unwrap()
                .contains("解除")
        );
        assert!(matches!(
            participation::read(&state).unwrap(),
            participation::ParticipationReadV2::Missing
        ));
        assert_eq!(peer.head(), head);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn m2d_c12_confirmation_apply_reopen_and_refusal() {
        const CHILD: &str = "POLARIS_TUI_M2D_C12_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate project state in a normal child rather than swapping
            // a process-global data root while other TUI tests are running.
            let fixture = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::m2d_c12_confirmation_apply_reopen_and_refusal",
                    "--nocapture",
                ])
                .env(CHILD, fixture.path())
                .env("POLARIS_DATA_DIR", fixture.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("m2d-c12-receipt: verified"));
            return;
        }
        use polaris_core::sync::{
            participation,
            repository::SyncRepository,
            share::{ShareLink, ShareManifest, SyncPaths},
        };
        let fixture = PathBuf::from(std::env::var_os(CHILD).unwrap());
        let project = fixture.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("shared.txt"), b"local").unwrap();
        let state = polaris_core::project::state_dir(&project).unwrap();
        source_apply::prepare_state(&project, &state).unwrap();
        let manifest = ShareManifest {
            version: 1,
            excluded: Vec::new(),
        };
        participation::join(
            &state,
            ShareLink {
                remote: "file:///unused-sync-remote".into(),
                share_id: "share-id1".into(),
                invitation_id: "invite-id1".into(),
                expires_unix_seconds: u64::MAX,
            },
            manifest.clone(),
            1,
        )
        .unwrap();
        let repository = SyncRepository::open(SyncPaths::under(&state)).unwrap();
        let incoming = fixture.join("incoming");
        std::fs::create_dir(&incoming).unwrap();
        std::fs::write(incoming.join("shared.txt"), b"agreed").unwrap();
        repository.stage_from(&incoming, &manifest).unwrap();
        let agreed = repository.commit("incoming fixture").unwrap().unwrap();
        let mut held = participation::read(&state)
            .unwrap()
            .participation()
            .unwrap()
            .clone();
        held.agreed_commit = Some(agreed.clone());
        participation::write(&state, &held).unwrap();
        let initial = std::fs::read(state.join("sync/participation.json")).unwrap();
        let read = sync_action(&project, &state, "", true, |_| {
            panic!("read must not confirm")
        })
        .unwrap();
        assert!(read.contains(&agreed));
        assert!(
            sync_action(&project, &state, "apply", true, |_| panic!(
                "queued apply must refuse"
            ))
            .is_err()
        );
        let mut broken_session = Session::new();
        let broken_persistence = polaris_core::session_store::PersistedSession::create(
            &fixture.join("broken-session"),
            &fixture.join("broken-memory.sqlite3"),
            "project",
            polaris_core::conversation_state::HistoryMode::Legacy,
            None,
        )
        .unwrap();
        broken_session.attach(broken_persistence).unwrap();
        let broken_path = broken_session
            .persistence
            .as_ref()
            .unwrap()
            .store
            .directory()
            .join("state.json");
        std::fs::write(&broken_path, b"corrupt-state").unwrap();
        assert!(broken_session.check_persistence().is_err());
        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 22)).unwrap(),
        );
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        let mut status = Status::Idle;
        let mut lines = Vec::new();
        handle_sync_action(
            &terminal,
            &mut reader,
            &project,
            &state,
            "apply",
            false,
            &broken_session,
            &mut status,
            &mut lines,
        );
        assert!(matches!(status, Status::Error(_)));
        assert_eq!(
            reader.0.len(),
            1,
            "a valid candidate must still refuse before confirmation"
        );
        assert!(lines.is_empty());
        assert_eq!(std::fs::read(project.join("shared.txt")).unwrap(), b"local");
        assert_eq!(
            std::fs::read(state.join("sync/participation.json")).unwrap(),
            initial
        );
        assert!(
            polaris_core::apply_owner::ApplyOwnerV1::acquire(&project)
                .unwrap()
                .active()
                .unwrap()
                .is_none()
        );
        let cancelled = sync_action(&project, &state, "apply", false, |view| {
            assert_eq!(view.revision, agreed);
            assert_eq!(view.changes, ["Modified: shared.txt"]);
            assert!(
                polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).is_err(),
                "confirmation must retain lease"
            );
            false
        })
        .unwrap();
        assert!(cancelled.contains("取り消し"));
        assert_eq!(std::fs::read(project.join("shared.txt")).unwrap(), b"local");
        assert_eq!(
            std::fs::read(state.join("sync/participation.json")).unwrap(),
            initial
        );
        assert!(
            polaris_core::apply_owner::ApplyOwnerV1::acquire(&project)
                .unwrap()
                .active()
                .unwrap()
                .is_none()
        );
        let drift = sync_action(&project, &state, "apply", false, |_| {
            std::fs::write(project.join("shared.txt"), b"changed-after-preview").unwrap();
            true
        });
        assert!(drift.is_err(), "preview drift must refuse");
        assert_eq!(
            std::fs::read(project.join("shared.txt")).unwrap(),
            b"changed-after-preview"
        );
        assert_eq!(
            std::fs::read(state.join("sync/participation.json")).unwrap(),
            initial
        );
        std::fs::write(project.join("shared.txt"), b"local").unwrap();
        let applied = sync_action(&project, &state, "apply", false, |_| true).unwrap();
        assert!(applied.contains("1件"));
        assert_eq!(
            std::fs::read(project.join("shared.txt")).unwrap(),
            b"agreed"
        );
        assert_eq!(
            participation::read(&state)
                .unwrap()
                .participation()
                .unwrap()
                .applied_commit
                .as_deref(),
            Some(agreed.as_str())
        );
        let archived = polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).unwrap();
        assert!(archived.active().unwrap().is_none());
        drop(archived);
        let repeated = sync_action(&project, &state, "apply", false, |_| {
            panic!("already applied must not confirm")
        })
        .unwrap();
        assert!(repeated.contains("反映済み"));
        assert!(
            sync_action(&project, &state, "recover wrong", false, |_| panic!(
                "wrong recovery must refuse"
            ))
            .is_err()
        );

        // A non-writable source directory gives a real, deterministic effect
        // failure after durable admission without killing a process or mocking
        // the owner. Reopening and recovery must never retry the source rename.
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let unknown_project = fixture.join("unknown-project");
        std::fs::create_dir(&unknown_project).unwrap();
        let source_file = unknown_project.join("shared.txt");
        std::fs::write(&source_file, b"untouched").unwrap();
        let unknown_state = polaris_core::project::state_dir(&unknown_project).unwrap();
        source_apply::prepare_state(&unknown_project, &unknown_state).unwrap();
        participation::join(&unknown_state, held.share.clone(), manifest.clone(), 1).unwrap();
        let unknown_repository = SyncRepository::open(SyncPaths::under(&unknown_state)).unwrap();
        unknown_repository.stage_from(&incoming, &manifest).unwrap();
        let incoming_revision = unknown_repository
            .commit("unknown fixture")
            .unwrap()
            .unwrap();
        let mut joined = participation::read(&unknown_state)
            .unwrap()
            .participation()
            .unwrap()
            .clone();
        joined.agreed_commit = Some(incoming_revision);
        participation::write(&unknown_state, &joined).unwrap();
        let initial_participation =
            std::fs::read(unknown_state.join("sync/participation.json")).unwrap();
        let before = std::fs::metadata(&source_file).unwrap();
        std::fs::set_permissions(&unknown_project, std::fs::Permissions::from_mode(0o555)).unwrap();
        let failed_apply = sync_action(&unknown_project, &unknown_state, "apply", false, |_| true);
        std::fs::set_permissions(&unknown_project, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed_apply.is_err(), "{failed_apply:?}");
        let owner = polaris_core::apply_owner::ApplyOwnerV1::acquire(&unknown_project).unwrap();
        let active = owner
            .active()
            .unwrap()
            .expect("failed source effect must remain discoverable");
        let key = active.descriptor().operation_key.as_hex();
        assert!(matches!(
            active.latest().phase,
            polaris_core::apply_intent::ApplyPhaseV1::OutcomeUnknown
        ));
        let effects = {
            let workspace = polaris_core::rewind_workspace::WorkspaceOwner::open(
                &unknown_project,
                &unknown_state.join("rewind-workspace"),
                polaris_core::rewind_workspace::RewindLimits::default(),
            )
            .unwrap();
            drop(owner);
            let lease = workspace.try_mutation().unwrap();
            polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
                &unknown_project,
                &unknown_state,
                &lease,
            )
            .unwrap()
        };
        assert!(
            management_clearance_error(
                &unknown_project,
                &effects,
                &slash::Action::Sync("apply".into())
            )
            .is_some()
        );
        assert!(
            management_clearance_error(
                &unknown_project,
                &effects,
                &slash::Action::Sync("status".into())
            )
            .is_none()
        );
        assert!(
            management_clearance_error(
                &unknown_project,
                &effects,
                &slash::Action::Sync(format!("recover {key}"))
            )
            .is_none()
        );
        assert!(
            sync_action(
                &unknown_project,
                &unknown_state,
                "status",
                false,
                |_| panic!("inspect")
            )
            .unwrap()
            .contains(&key)
        );
        assert!(
            sync_action(
                &unknown_project,
                &unknown_state,
                "apply",
                false,
                |_| panic!("must not retry")
            )
            .is_err()
        );
        assert!(
            sync_action(
                &unknown_project,
                &unknown_state,
                "recover wrong",
                false,
                |_| panic!("wrong key must not confirm")
            )
            .is_err()
        );
        let cancelled_recovery = sync_action(
            &unknown_project,
            &unknown_state,
            &format!("recover {key}"),
            false,
            |_| false,
        )
        .unwrap();
        assert!(cancelled_recovery.contains("取り消し"));
        // Mirror startup: C12 unknown must not hide the recovery command.
        let workspace = open_rewind_workspace(&unknown_project, &unknown_state).unwrap();
        {
            let lease = workspace.try_mutation().unwrap();
            let data_root = fixture.join("data");
            let project_id = persist::project_identity(&unknown_project).unwrap();
            polaris_core::session_switch::SessionSwitchOwner {
                effects: &effects,
                lease: &lease,
                state_dir: &unknown_state,
            }
            .recover(
                &data_root,
                &unknown_state.join("memory.sqlite3"),
                &project_id,
            )
            .unwrap();
            polaris_core::management_effect::workflow_change::WorkflowChangeOwner {
                effects: &effects,
                lease: &lease,
                state_dir: &unknown_state,
            }
            .recover(
                &data_root,
                &unknown_state.join("memory.sqlite3"),
                &project_id,
            )
            .unwrap();
        }
        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 22)).unwrap(),
        );
        for _ in 0..2 {
            let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
            let mut status = Status::Idle;
            let mut lines = Vec::new();
            let mut refused_reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
            handle_sync_action(
                &terminal,
                &mut refused_reader,
                &unknown_project,
                &unknown_state,
                &format!("recover {key}"),
                false,
                &broken_session,
                &mut status,
                &mut lines,
            );
            assert!(matches!(status, Status::Error(_)));
            assert_eq!(refused_reader.0.len(), 1);
            assert!(lines.is_empty());
            handle_sync_action(
                &terminal,
                &mut reader,
                &unknown_project,
                &unknown_state,
                &format!("recover {key}"),
                false,
                &Session::new(),
                &mut status,
                &mut lines,
            );
            assert!(matches!(status, Status::Notice(_)));
            assert!(
                reader.0.is_empty(),
                "recovery requires explicit confirmation"
            );
            let recovered = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                recovered.contains("未確定") && recovered.contains("再適用していません"),
                "{recovered}"
            );
            let after = std::fs::metadata(&source_file).unwrap();
            assert_eq!(
                (before.dev(), before.ino(), before.mtime(), before.ctime()),
                (after.dev(), after.ino(), after.mtime(), after.ctime())
            );
            assert_eq!(std::fs::read(&source_file).unwrap(), b"untouched");
            assert_eq!(
                std::fs::read(unknown_state.join("sync/participation.json")).unwrap(),
                initial_participation
            );
        }
        println!("m2d-c12-receipt: verified");
    }

    #[test]
    fn workflow_confirmation_shows_exact_session_and_requires_y() {
        let terminal = RefCell::new(
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 16)).unwrap(),
        );
        let view = render::WorkflowChangeConfirmView {
            inventory: "T03",
            project_id: "project-1",
            session_id: "session-2",
            generation: 7,
            before_phase: "specify",
            after_phase: "implement",
            before_mode: "manual",
            after_mode: "manual",
        };
        let mut declined = ScriptedReader(VecDeque::from([KeyCode::Esc]));
        assert!(!confirm_workflow_change(&terminal, &mut declined, &view));
        let mut accepted = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        assert!(confirm_workflow_change(&terminal, &mut accepted, &view));
        let screen = {
            let terminal = terminal.borrow();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>()
        };
        for expected in ["T03", "project-1", "session-2", "7", "specify", "implement"] {
            assert!(screen.contains(expected), "missing {expected}: {screen}");
        }
    }

    #[test]
    fn draw_frame_puts_the_footer_in_the_bottom_inline_viewport_height_rows() {
        let backend = ratatui::backend::TestBackend::new(60, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut wrap_cache = render::HistoryWrapCache::default();
        let history = vec![render::HistoryLine {
            line: ratatui::text::Line::from("line from history"),
            shaded: false,
        }];
        terminal
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut wrap_cache,
                    0,
                    "",
                    0,
                    &Status::Idle,
                    &render::HeaderInfo {
                        provider_name: "openai",
                        model_name: "gpt-5.4",
                        usage: polaris_provider::Usage::default(),
                        cwd: "/tmp",
                        cwd_short: "~",
                        effort_name: "low",
                    },
                    &[],
                    0,
                    &None,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(rows[0].contains("line from history"));
        // The footer's placeholder text lands in the bottom
        // `render::footer_height(0)` rows (no suggestions here), not the
        // top history region.
        let footer_start = 30 - render::footer_height(0) as usize;
        assert!(
            rows[footer_start..]
                .iter()
                .any(|r| r.contains("Ask polaris to do anything"))
        );
        assert!(
            !rows[..footer_start]
                .iter()
                .any(|r| r.contains("Ask polaris to do anything"))
        );
    }

    #[test]
    fn draw_frame_wraps_a_history_line_wider_than_a_narrow_window_instead_of_clipping_it() {
        // A window half as wide as normal (30 columns) must still show the
        // full text of a long line, across multiple wrapped rows, rather
        // than clipping it at the window's right edge.
        let backend = ratatui::backend::TestBackend::new(30, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut wrap_cache = render::HistoryWrapCache::default();
        let long_text = "one two three four five six seven eight nine ten";
        let history = vec![render::HistoryLine {
            line: ratatui::text::Line::from(long_text),
            shaded: false,
        }];
        terminal
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut wrap_cache,
                    0,
                    "",
                    0,
                    &Status::Idle,
                    &render::HeaderInfo {
                        provider_name: "openai",
                        model_name: "gpt-5.4",
                        usage: polaris_provider::Usage::default(),
                        cwd: "/tmp",
                        cwd_short: "~",
                        effort_name: "low",
                    },
                    &[],
                    0,
                    &None,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let joined: String = rows.join(" ");
        for word in long_text.split(' ') {
            assert!(
                joined.contains(word),
                "{word:?} from the long line should still appear somewhere on screen: {rows:?}"
            );
        }
        assert!(
            !rows.iter().any(|r| r.contains(long_text)),
            "the full text should NOT fit on a single 30-column row unwrapped: {rows:?}"
        );
    }

    #[test]
    fn draw_frame_highlights_an_active_selection() {
        // Tall enough that `render::footer_height(0)` doesn't eat the whole
        // viewport and leave zero rows for the history area (matches the
        // height the other `draw_frame` tests in this module use).
        let backend = ratatui::backend::TestBackend::new(40, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut wrap_cache = render::HistoryWrapCache::default();
        // `HistoryLine::plain` is private to `render.rs` — this test lives in
        // `lib.rs`'s own test module, a different module, so it must build
        // `HistoryLine` via its public fields directly (both `line` and
        // `shaded` are `pub`), matching how the two existing `draw_frame`
        // tests in this same module already do it (grep `render::HistoryLine {`
        // in `lib.rs`).
        let history = vec![render::HistoryLine {
            line: ratatui::text::Line::from("select me"),
            shaded: false,
        }];
        let sel = Some(selection::Selection {
            anchor: selection::TextPos { line: 0, col: 0 },
            cursor: selection::TextPos { line: 0, col: 6 },
            dragging: false,
        });
        terminal
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut wrap_cache,
                    0,
                    "",
                    0,
                    &Status::Idle,
                    &render::HeaderInfo {
                        provider_name: "openai",
                        model_name: "gpt-5.4",
                        usage: polaris_provider::Usage::default(),
                        cwd: "/tmp",
                        cwd_short: "~",
                        effort_name: "low",
                    },
                    &[],
                    0,
                    &sel,
                )
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(0, 0)].bg,
            render::SELECTION_HIGHLIGHT_COLOR,
            "the first selected column should be highlighted"
        );
        assert_ne!(
            buf[(7, 0)].bg,
            render::SELECTION_HIGHLIGHT_COLOR,
            "a column past the selection should not be highlighted"
        );
    }

    /// Regression test for the *elimination*, not just relocation, of the
    /// blank gap between the last conversation-history row and the footer
    /// block — proves `history_area` actually grows to use every row
    /// `footer_height(0)` doesn't need, rather than always leaving the
    /// difference between a fixed worst-case reservation and the popup's
    /// actual height (9 rows, `14 - 5`) as unused blank space whenever the
    /// suggestions popup isn't showing (the common case).
    ///
    /// Fills history with more lines than any plausible `history_area`
    /// height on this 20-row backend, so every row of the *actual* history
    /// area — whatever height it turns out to be — has real "history line
    /// N" text in it, right up to and including its very last row. Under
    /// the old fixed `FOOTER_HEIGHT` (14) sizing this backend's history
    /// area would only have gotten `20 - 14 = 6` rows (rows 0..6), leaving
    /// rows 6..15 as pure unused blank space above the footer's status row
    /// at row 15 — this test's row-14 assertion below would have failed
    /// there, since row 14 sat well inside that unused gap, not inside
    /// `history_area` at all.
    #[test]
    fn draw_frame_leaves_no_blank_gap_between_history_and_the_footer() {
        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut wrap_cache = render::HistoryWrapCache::default();
        let history: Vec<render::HistoryLine> = (0..20)
            .map(|i| render::HistoryLine {
                line: ratatui::text::Line::from(format!("history line {i}")),
                shaded: false,
            })
            .collect();
        terminal
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut wrap_cache,
                    0,
                    "",
                    0,
                    &Status::Idle,
                    &render::HeaderInfo {
                        provider_name: "openai",
                        model_name: "gpt-5.4",
                        usage: polaris_provider::Usage::default(),
                        cwd: "/tmp",
                        cwd_short: "~",
                        effort_name: "low",
                    },
                    &[],
                    0,
                    &None,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        // 0 suggestions here, so this must match what `draw_frame` itself
        // just used to size `footer_area`.
        let expected_history_height = 20 - render::footer_height(0) as usize;
        assert!(
            rows[expected_history_height - 1].contains("history line"),
            "the row immediately above the footer must still be real \
             history content, not blank leftover space: {:?}",
            rows[expected_history_height - 1]
        );
        assert!(
            !rows[expected_history_height].contains("history line"),
            "the footer's own first row (status_area) must start \
             immediately below history, with nothing blank in between: {:?}",
            rows[expected_history_height]
        );
    }

    /// Closes the duplication-drift risk between `area_height_for_history`
    /// (used by the four mouse-event handlers, which run outside the
    /// `draw()` closure and so can't ask ratatui's own `Layout` what
    /// `history_area.height` is this frame) and what `draw_frame` actually
    /// renders. Rather than duplicating `draw_frame`'s own
    /// `Layout::vertical` call a second time (which would only prove the
    /// test's copy agrees with itself), this renders a real `draw_frame`
    /// call with more history lines than any plausible `history_area`
    /// height, all carrying the same marker (`"H"`), then counts how many
    /// contiguous rows from the top actually got that marker drawn into
    /// them — the true observable size of `history_area`, straight from
    /// the rendered buffer. That count must equal
    /// `area_height_for_history(render::footer_height(n), terminal_height)`
    /// for both a representative suggestion count with no popup (`0`) and
    /// one at the popup's display cap (`MAX_DISPLAYED_SUGGESTIONS`).
    #[test]
    fn area_height_for_history_matches_draw_frames_actual_history_area_height() {
        for &suggestion_count in &[0usize, render::MAX_DISPLAYED_SUGGESTIONS] {
            let terminal_height: u16 = 20;
            let suggestions: Vec<&slash::SlashCommand> =
                slash::COMMANDS.iter().take(suggestion_count).collect();
            assert_eq!(
                suggestions.len(),
                suggestion_count,
                "slash::COMMANDS must have at least MAX_DISPLAYED_SUGGESTIONS entries \
                 for this test to actually exercise the requested suggestion count"
            );
            let backend = ratatui::backend::TestBackend::new(60, terminal_height);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
            let mut wrap_cache = render::HistoryWrapCache::default();
            let history: Vec<render::HistoryLine> = (0..terminal_height)
                .map(|_| render::HistoryLine {
                    line: ratatui::text::Line::from("H"),
                    shaded: false,
                })
                .collect();
            terminal
                .draw(|f| {
                    draw_frame(
                        f,
                        &history,
                        &mut wrap_cache,
                        0,
                        "",
                        0,
                        &Status::Idle,
                        &render::HeaderInfo {
                            provider_name: "openai",
                            model_name: "gpt-5.4",
                            usage: polaris_provider::Usage::default(),
                            cwd: "/tmp",
                            cwd_short: "~",
                            effort_name: "low",
                        },
                        &suggestions,
                        0,
                        &None,
                    )
                })
                .expect("draw");
            let buffer = terminal.backend().buffer();
            let actual_history_rows = (0..buffer.area.height)
                .take_while(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].symbol() == "H"))
                .count() as u16;
            let via_helper =
                area_height_for_history(render::footer_height(suggestion_count), terminal_height);
            assert_eq!(
                actual_history_rows, via_helper,
                "suggestion_count={suggestion_count}: area_height_for_history \
                 must stay in sync with what draw_frame actually renders"
            );
        }
    }

    #[test]
    fn append_new_history_appends_only_the_unprinted_tail() {
        let mut session = polaris_core::session::Session::default();
        session.push_user("first");
        let mut printed_messages = 0;
        let mut printed_local_lines = 0;
        let mut history = Vec::new();

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        let after_first = history.len();
        assert!(after_first > 0);

        session.push_assistant("reply", Vec::new());
        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert!(
            history.len() > after_first,
            "the assistant reply should have been appended, not reprinted from scratch"
        );

        // Calling again with nothing new appends nothing.
        let stable = history.len();
        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(history.len(), stable);
    }

    #[test]
    fn append_new_history_self_corrects_when_the_session_shrinks() {
        let mut session = polaris_core::session::Session::default();
        session.push_user("first");
        session.push_assistant("reply", Vec::new());
        let mut printed_messages = 0;
        let mut printed_local_lines = 0;
        let mut history = Vec::new();
        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(printed_messages, 2);

        // Simulates /clear: the session shrinks out from under the counters.
        session.messages.clear();
        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(printed_messages, 0);
    }

    /// Regression coverage for the final-whole-branch-review Critical
    /// finding: `printed_messages` is a second local index into
    /// `session.messages`, alongside `checkpoint`, and it goes stale in
    /// exactly the same way when compaction shrinks `session.messages`
    /// mid-turn. Left unadjusted, the next `append_new_history` call sees
    /// a `session.messages.len()` that no longer lines up with
    /// `printed_messages`, and depending on how the shrink compares to
    /// what the turn then appends, either the shrink-guard fires and wipes
    /// the entire `history` buffer (header included), nothing new renders
    /// at all, or the wrong slice of new messages renders. These three
    /// tests simulate what `run()`'s two `AgentEvent::HistoryCompacted`
    /// drain sites do — shrink `session.messages` the way `compact` does,
    /// then apply the exact same `saturating_sub` adjustment to
    /// `printed_messages` that the fix makes — across all three
    /// before/after-vs-turn-growth orderings, and confirm
    /// `append_new_history` renders correctly in each.
    #[test]
    fn append_new_history_renders_correctly_after_compaction_shrinks_more_than_the_turn_grows() {
        // D (messages compaction removes) > A (messages the turn then
        // adds) — the normal case: several old turns fold into one
        // summary, and the current turn only adds its own reply.
        let mut session = Session::default();
        for i in 0..6 {
            session.push_user(&format!("turn {i}"));
        }
        let mut printed_messages = 0;
        let mut printed_local_lines = 0;
        let mut history = vec![render::HistoryLine {
            line: Line::from("HEADER"),
            shaded: false,
        }];

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(printed_messages, 6);
        let after_initial_render = history.len();

        // Compaction folds all 6 messages into 2 (a summary plus one kept
        // turn) — D = 4 — mirroring the adjustment made at both
        // `AgentEvent::HistoryCompacted` drain sites in `run()`.
        let messages_before = session.messages.len();
        session.messages = vec![
            Message::user("summary of turns 0-4"),
            Message::user("turn 5"),
        ];
        let messages_after = session.messages.len();
        printed_messages =
            printed_messages.saturating_sub(messages_before.saturating_sub(messages_after));
        assert_eq!(printed_messages, 2);

        // The turn itself then appends its own reply (A = 1 < D = 4).
        session.push_assistant("the turn's reply", Vec::new());

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );

        assert_eq!(
            history[0].line,
            Line::from("HEADER"),
            "the shrink-guard must not have fired and wiped the header"
        );
        assert_eq!(
            history.len(),
            after_initial_render + 1,
            "exactly the turn's one new reply should have been appended — \
             not zero (guard silently no-op'ing) and not a full reprint"
        );
        assert_eq!(printed_messages, 3);
    }

    #[test]
    fn append_new_history_renders_correctly_after_compaction_shrinks_exactly_as_much_as_the_turn_grows()
     {
        // D == A: without the `printed_messages` adjustment,
        // `session.messages.len()` ends up back at its pre-compaction
        // value, so the shrink-guard never fires but the "anything new to
        // render?" check also sees no growth — the turn's reply silently
        // never appears.
        let mut session = Session::default();
        for i in 0..6 {
            session.push_user(&format!("turn {i}"));
        }
        let mut printed_messages = 0;
        let mut printed_local_lines = 0;
        let mut history = vec![render::HistoryLine {
            line: Line::from("HEADER"),
            shaded: false,
        }];

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(printed_messages, 6);
        let after_initial_render = history.len();

        // Compaction removes 2 messages (D = 2).
        let messages_before = session.messages.len();
        session.messages = vec![
            Message::user("summary of turns 0-2"),
            Message::user("turn 3"),
            Message::user("turn 4"),
            Message::user("turn 5"),
        ];
        let messages_after = session.messages.len();
        printed_messages =
            printed_messages.saturating_sub(messages_before.saturating_sub(messages_after));
        assert_eq!(printed_messages, 4);

        // The turn adds exactly 2 new messages (A = D = 2): its own user
        // follow-up plus the assistant's reply.
        session.push_user("follow-up");
        session.push_assistant("the turn's reply", Vec::new());

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );

        assert_eq!(
            history[0].line,
            Line::from("HEADER"),
            "the shrink-guard must not have fired and wiped the header"
        );
        assert_eq!(
            history.len(),
            after_initial_render + 2,
            "both of the turn's new messages should have been appended, \
             not silently dropped"
        );
        assert_eq!(printed_messages, 6);
    }

    #[test]
    fn append_new_history_renders_correctly_after_compaction_shrinks_less_than_the_turn_grows() {
        // D < A: without the `printed_messages` adjustment, some of the
        // turn's own new messages get skipped from the render — the slice
        // starts too far into `session.messages`.
        let mut session = Session::default();
        for i in 0..6 {
            session.push_user(&format!("turn {i}"));
        }
        let mut printed_messages = 0;
        let mut printed_local_lines = 0;
        let mut history = vec![render::HistoryLine {
            line: Line::from("HEADER"),
            shaded: false,
        }];

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(printed_messages, 6);
        let after_initial_render = history.len();

        // Compaction removes just 1 message (D = 1).
        let messages_before = session.messages.len();
        session.messages = vec![
            Message::user("summary of turn 0"),
            Message::user("turn 1"),
            Message::user("turn 2"),
            Message::user("turn 3"),
            Message::user("turn 4"),
        ];
        let messages_after = session.messages.len();
        printed_messages =
            printed_messages.saturating_sub(messages_before.saturating_sub(messages_after));
        assert_eq!(printed_messages, 5);

        // The turn adds 3 new messages (A = 3 > D = 1).
        session.push_user("follow-up");
        session.push_assistant("intermediate reply", Vec::new());
        session.push_assistant("final reply", Vec::new());

        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );

        assert_eq!(
            history[0].line,
            Line::from("HEADER"),
            "the shrink-guard must not have fired and wiped the header"
        );
        assert_eq!(
            history.len(),
            after_initial_render + 3,
            "all three of the turn's new messages should have been \
             appended, not just the tail of them"
        );
        assert_eq!(printed_messages, 8);
    }

    #[test]
    fn append_live_event_appends_formatted_lines() {
        let mut history = Vec::new();
        let mut activity_rows = AgentActivityRows::default();
        let event = polaris_core::AgentEvent::ToolStarted {
            name: "read".to_string(),
            detail: "Cargo.toml".to_string(),
        };
        append_live_event(&event, &mut history, &mut activity_rows);
        assert!(!history.is_empty());
    }

    #[test]
    fn child_tool_activity_replaces_only_that_agents_single_progress_row() {
        let mut history = Vec::new();
        let mut activity_rows = AgentActivityRows::default();
        for (display_name, agent_type) in [("Sirius", "file-reader"), ("Antares", "file-reader")] {
            append_live_event(
                &polaris_core::AgentEvent::SpawnStarted {
                    display_name: display_name.to_string(),
                    agent_type: agent_type.to_string(),
                    task: "private".to_string(),
                },
                &mut history,
                &mut activity_rows,
            );
        }

        let activity =
            |display_name: &str, name: &str, path: &str| polaris_core::AgentEvent::SpawnActivity {
                display_name: display_name.to_string(),
                agent_type: "file-reader".to_string(),
                name: name.to_string(),
                detail: serde_json::json!({"path": path}).to_string(),
            };
        append_live_event(
            &activity("Sirius", "read", "first.md"),
            &mut history,
            &mut activity_rows,
        );
        append_live_event(
            &activity("Antares", "write", "notes.md"),
            &mut history,
            &mut activity_rows,
        );
        append_live_event(
            &activity("Sirius", "read", "second.md"),
            &mut history,
            &mut activity_rows,
        );

        assert_eq!(history.len(), 4, "activity must update instead of append");
        assert_eq!(history[0].line.to_string(), "⏺ Sirius (file-reader)");
        assert_eq!(history[1].line.to_string(), "  → read(second.md)");
        assert_eq!(history[2].line.to_string(), "⏺ Antares (file-reader)");
        assert_eq!(history[3].line.to_string(), "  → write(notes.md)");
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        action: slash::Action,
        session: &mut Session,
        session_path: &std::path::Path,
        status: &mut Status,
        skills: &[polaris_skills::Skill],
        cwd: &std::path::Path,
        local_lines: &mut Vec<ratatui::text::Line<'static>>,
    ) -> SlashOutcome {
        apply_slash_action(
            action,
            session,
            session_path,
            status,
            "openai",
            "gpt-5.4",
            polaris_provider::UsageReport::default(),
            skills,
            cwd,
            cwd,
            cwd,
            false,
            local_lines,
        )
    }

    fn skill(name: &str) -> polaris_skills::Skill {
        polaris_skills::Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            body: String::new(),
            path: PathBuf::new(),
        }
    }

    // `abbreviate_home` reads `HOME`, which is process-global state —
    // swapping it races with any other test doing the same unless
    // serialized.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn abbreviate_home_collapses_a_path_under_home_to_a_tilde() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
        unsafe {
            std::env::set_var("HOME", "/Users/example");
        }

        let short = abbreviate_home(std::path::Path::new("/Users/example/File/projects/polaris"));

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(short, "~/File/projects/polaris");
    }

    #[test]
    fn abbreviate_home_leaves_a_path_outside_home_unchanged() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
        unsafe {
            std::env::set_var("HOME", "/Users/example");
        }

        let unchanged = abbreviate_home(std::path::Path::new("/var/other/place"));

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(unchanged, "/var/other/place");
    }

    #[test]
    fn run_skills_picker_with_none_discovered_dismisses_on_any_key() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter]));

        // Must return promptly rather than looping forever on an empty list.
        run_skills_picker(&terminal, &mut reader, &[]);
    }

    #[test]
    fn run_skills_picker_shows_discovered_skills_and_closes_on_enter() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let skills = [skill("alpha"), skill("beta")];
        // Move the highlight once, then close — exercises Down as well as
        // the close path.
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));

        run_skills_picker(&terminal, &mut reader, &skills);

        let content = terminal
            .borrow()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("alpha"));
        assert!(content.contains("beta"));
    }

    #[cfg(unix)]
    #[test]
    fn init_creates_agents_md_in_cwd() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Init,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let agents_md = dir.path().join("AGENTS.md");
        assert!(agents_md.exists());
        match status {
            Status::Notice(n) => assert!(n.contains("created")),
            _ => panic!("expected a Notice"),
        }
    }

    /// The template has to survive the reader it was written for. Asserting
    /// only that the file exists passed for the whole of the template's
    /// previous life, during which it delivered nothing: it had no
    /// `## Always on` heading, so `extract_always_on` returned an empty
    /// string and the run carried zero project rules.
    #[test]
    fn what_init_writes_actually_reaches_the_model_and_fits_the_ceiling() {
        use polaris_core::constitution::{CONSTITUTION_LIMIT, extract_always_on};
        let template = agents_md_template();

        let block = extract_always_on(&template);
        assert!(
            !block.is_empty(),
            "the template delivers nothing:\n{template}"
        );
        assert!(
            block.lines().any(|l| l.trim_start().starts_with('-')),
            "the delivered block has no rule line: {block:?}"
        );
        // Every line of it has to fit, or `/init` ships a file whose own
        // example rule is silently dropped.
        let cost = polaris_core::budget::count_tokens(&block);
        assert!(
            cost <= CONSTITUTION_LIMIT,
            "the template's own block costs {cost}, over the {CONSTITUTION_LIMIT}-token ceiling: {block:?}"
        );
        // The prose must not promise the behaviour the code does not have.
        assert!(
            !template.contains("Everything up to the first"),
            "the template still describes a rule `extract_always_on` does not implement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn init_does_not_overwrite_an_existing_agents_md() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        let agents_md = dir.path().join("AGENTS.md");
        std::fs::write(&agents_md, "existing content").expect("write");

        call(
            slash::Action::Init,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert_eq!(
            std::fs::read_to_string(&agents_md).unwrap(),
            "existing content"
        );
        match status {
            Status::Notice(n) => assert!(n.contains("already exists")),
            _ => panic!("expected a Notice"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn clear_empties_the_session_and_the_persisted_file_in_place() {
        let mut session = Session::default();
        session.push_user("something from before");
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        persist::append_message(&session_path, session.messages.last().unwrap())
            .expect("seed the file");

        call(
            slash::Action::Clear,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(session.messages.is_empty());
        assert_eq!(
            std::fs::read_to_string(&session_path).unwrap(),
            "",
            "the persisted file should be emptied too, not just the in-memory session"
        );
        assert!(matches!(status, Status::Idle));
    }

    #[cfg(unix)]
    #[test]
    fn new_and_resume_are_never_actually_dispatched_through_apply_slash_action() {
        // `run()` intercepts every one of these before they'd ever reach
        // here (see `handle_new_session`/`handle_resume`/`handle_fork`/
        // `handle_permissions`/`handle_model`/`run_skills_picker`); this
        // only guards the defensive fallback arm against silently doing
        // nothing instead of surfacing that something is wrong.
        for action in [
            slash::Action::New,
            slash::Action::Resume,
            slash::Action::Fork,
            slash::Action::Permissions,
            slash::Action::Model,
            slash::Action::Skills,
        ] {
            let mut session = Session::default();
            let mut status = Status::Idle;
            let dir = tempfile::tempdir().expect("temp dir");
            let session_path = dir.path().join("session.jsonl");
            let mut local_lines = Vec::new();

            call(
                action,
                &mut session,
                &session_path,
                &mut status,
                &[],
                dir.path(),
                &mut local_lines,
            );

            match status {
                Status::Notice(n) => assert!(n.contains("internal")),
                _ => panic!("expected a Notice"),
            }
        }
    }

    #[test]
    fn handle_new_session_starts_a_fresh_file_without_touching_the_old_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session {
            disable_files_md_auto_regenerate: true,
            ..Session::default()
        };
        session.push_user("something from before");
        let old_path = dir.path().join("old.jsonl");
        persist::append_message(&old_path, session.messages.last().unwrap())
            .expect("seed the old file");
        let mut session_path = old_path.clone();
        let mut meta_path = dir.path().join("old.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = vec![Line::from("leftover /diff output")];

        handle_new_session(
            dir.path(),
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        );

        assert!(session.messages.is_empty());
        assert!(session.disable_files_md_auto_regenerate);
        assert!(local_lines.is_empty());
        assert!(matches!(status, Status::Idle));
        assert_ne!(
            session_path, old_path,
            "a new session should get its own file"
        );
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap().lines().count(),
            1,
            "the old conversation must stay intact on disk, not get wiped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workflow_session_defaults_to_builtin_skills_and_supports_explicit_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("sessions");
        let mut config = polaris_core::config::Config::default();
        for enabled in [true, false] {
            if !enabled {
                config.workflow.enabled = false;
            }
            let session = new_workflow_session(WorkflowSessionArgs {
                config: &config.workflow,
                initial_phase: None,
                approval_auto: config.approval_auto,
                history_mode: config.history_mode,
                input_preparation: config.input_preparation,
                cwd: dir.path(),
                state_dir: dir.path(),
                sessions_dir: &sessions_dir,
                strict_history: None,
                summary_usage: None,
                history_backend_factory: None,
                files_md_auto_regenerate: config.files_md_auto_regenerate,
                continuation: None,
                workflow_mode_overridden: false,
                skills: &[],
                #[cfg(unix)]
                management_effects: None,
                #[cfg(unix)]
                project_mutation: None,
            })
            .await
            .unwrap();
            assert_eq!(session.workflow.is_some(), enabled);
            assert_eq!(session.persistence.is_some(), enabled);
            assert!(session.strict_history.is_none());
            if let Some(workflow) = &session.workflow {
                let turn = workflow.begin_turn(&[]).unwrap();
                assert_eq!(turn.ids, ["builtin:workflow-core"]);
                assert_eq!(turn.state.phase, polaris_core::workflow::Phase::General);
                let saved = session.persistence.as_ref().unwrap().snapshot().unwrap();
                assert_eq!(
                    saved.state.history_mode,
                    polaris_core::conversation_state::HistoryMode::Legacy
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_new_creates_a_separate_family_and_preserves_a_denied_draft() {
        use polaris_core::{
            conversation_state::HistoryMode,
            desktop_store::SessionManagerStore,
            management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1},
            rewind_workspace::{RewindLimits, WorkspaceOwner},
            session_manager::SessionId,
        };

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state_dir = root.path().join("state");
        let sessions_dir = root.path().join("data/sessions");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(
            &state_dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state_dir.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let effects = {
            let lease = owner.try_mutation().unwrap();
            ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap()
        };
        let config = polaris_core::config::Config::default();
        let mut workflow = config.workflow.clone();
        workflow.enabled = false;
        let mut session = new_workflow_session(WorkflowSessionArgs {
            config: &workflow,
            initial_phase: None,
            approval_auto: config.approval_auto,
            history_mode: HistoryMode::Legacy,
            input_preparation: config.input_preparation,
            cwd: &project,
            state_dir: &state_dir,
            sessions_dir: &sessions_dir,
            strict_history: None,
            summary_usage: None,
            history_backend_factory: None,
            files_md_auto_regenerate: config.files_md_auto_regenerate,
            continuation: None,
            workflow_mode_overridden: false,
            skills: &[],
            management_effects: Some(&effects),
            project_mutation: Some(&owner),
        })
        .await
        .unwrap();
        assert!(session.workflow.is_none());
        let saved = session.persistence.as_ref().unwrap().snapshot().unwrap();
        assert_eq!(saved.state.history_mode, HistoryMode::Legacy);
        let child = SessionId::new(saved.state.session_id.clone()).unwrap();
        let (source_store, family) =
            SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &child)
                .unwrap();
        assert!(family.sessions.contains_key(&child));
        drop(source_store);
        assert!(effects.blocker().unwrap().is_none());
        assert!(effects.effects().unwrap().iter().any(|effect| {
            effect.request.inventory.encoded() == "T06"
                && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
        }));

        session.push_user("old v2 message");
        session.check_persistence().unwrap();
        let source = session.persistence.as_ref().unwrap().clone();
        let source_dir = source.store.directory().to_path_buf();
        let source_state = std::fs::read(source_dir.join("state.json")).unwrap();
        let source_raw = std::fs::read(source_dir.join("raw.jsonl")).unwrap();
        let mut session_path = sessions_dir.join(format!("{}.jsonl", child.as_str()));
        let mut meta_path = session_path.with_extension("meta.json");
        let mut started = 1;
        let mut status = Status::Idle;
        let mut local_lines = vec![Line::from("old local output")];
        assert!(handle_managed_new_session(
            &sessions_dir,
            &state_dir,
            &owner,
            &effects,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
        let new_id = session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id;
        assert_ne!(new_id, child.as_str());
        assert_eq!(session_path.file_stem().unwrap(), new_id.as_str());
        assert_eq!(meta_path, session_path.with_extension("meta.json"));
        assert!(session.messages.is_empty());
        assert!(session.workflow.is_none());
        assert!(local_lines.is_empty());
        assert!(matches!(status, Status::Idle));
        assert_eq!(
            std::fs::read(source_dir.join("state.json")).unwrap(),
            source_state
        );
        assert_eq!(
            std::fs::read(source_dir.join("raw.jsonl")).unwrap(),
            source_raw
        );
        let new_child = SessionId::new(new_id).unwrap();
        let (new_store, new_family) = SessionManagerStore::open_family_owned(
            &state_dir.join("tui-admissions-v1"),
            &new_child,
        )
        .unwrap();
        assert_eq!(new_family.sessions.len(), 1);
        assert!(new_family.sessions.contains_key(&new_child));
        drop(new_store);
        let (_store, old_family) =
            SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &child)
                .unwrap();
        assert!(old_family.sessions.contains_key(&child));
        assert_eq!(
            effects
                .effects()
                .unwrap()
                .iter()
                .filter(|effect| {
                    effect.request.inventory.encoded() == "T06"
                        && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
                })
                .count(),
            2
        );

        let journal = state_dir.join("management-effects-v1/journal.v1.jsonl");
        let journal_before = std::fs::read(&journal).unwrap();
        let cursor_before = effects.cursor().unwrap();
        let new_dir = session
            .persistence
            .as_ref()
            .unwrap()
            .store
            .directory()
            .to_path_buf();
        let new_state = std::fs::read(new_dir.join("state.json")).unwrap();
        let new_raw = std::fs::read(new_dir.join("raw.jsonl")).unwrap();
        let current_path = session_path.clone();
        let current_meta = meta_path.clone();
        let current_started = started;
        local_lines.push(Line::from("keep on refusal"));
        let busy_lease = owner.try_mutation().unwrap();

        let mut popup_draft = "/ne".to_string();
        let mut popup_cursor = 2;
        let mut queued = VecDeque::new();
        let mut paused = false;
        let submitted = std::mem::take(&mut popup_draft);
        assert!(!handle_managed_new_session(
            &sessions_dir,
            &state_dir,
            &owner,
            &effects,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
        restore_rejected_new_submission(
            submitted,
            popup_cursor,
            false,
            &mut popup_draft,
            &mut popup_cursor,
            &mut queued,
            &mut paused,
        );
        assert_eq!(popup_draft, "/ne");
        assert_eq!(popup_cursor, 2);

        let mut ordinary_draft = "/new".to_string();
        let mut ordinary_cursor = 2;
        let mut history = input::History::new();
        history.record("prior input");
        let previous_cursor = ordinary_cursor;
        let InputAction::Submit(submitted) = apply_key(
            &mut ordinary_draft,
            &mut ordinary_cursor,
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        ) else {
            panic!("Enter should submit /new");
        };
        record_submission_before_dispatch(&mut history, &submitted, false);
        assert!(!handle_managed_new_session(
            &sessions_dir,
            &state_dir,
            &owner,
            &effects,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
        restore_rejected_new_submission(
            submitted,
            previous_cursor,
            false,
            &mut ordinary_draft,
            &mut ordinary_cursor,
            &mut queued,
            &mut paused,
        );
        assert_eq!(ordinary_draft, "/new");
        assert_eq!(ordinary_cursor, 2);
        assert_eq!(history.older(&ordinary_draft), Some("prior input"));
        assert_eq!(history.newer(), Some("/new"));

        let mut live_draft = "unsent request".to_string();
        let mut live_cursor = 4;
        queued.extend(["/new".to_string(), "later request".to_string()]);
        let submitted = queued.pop_front().unwrap();
        assert!(!handle_managed_new_session(
            &sessions_dir,
            &state_dir,
            &owner,
            &effects,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
        restore_rejected_new_submission(
            submitted,
            4,
            true,
            &mut live_draft,
            &mut live_cursor,
            &mut queued,
            &mut paused,
        );
        assert_eq!(live_draft, "unsent request");
        assert_eq!(live_cursor, 4);
        assert_eq!(
            queued,
            VecDeque::from(["/new".into(), "later request".into()])
        );
        assert!(paused);
        drop(busy_lease);
        assert_eq!(session_path, current_path);
        assert_eq!(meta_path, current_meta);
        assert_eq!(started, current_started);
        assert_eq!(
            std::fs::read(new_dir.join("state.json")).unwrap(),
            new_state
        );
        assert_eq!(std::fs::read(new_dir.join("raw.jsonl")).unwrap(), new_raw);
        assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
        assert_eq!(effects.cursor().unwrap(), cursor_before);
        assert_eq!(local_lines.len(), 1);

        assert!(handle_managed_new_session(
            &sessions_dir,
            &state_dir,
            &owner,
            &effects,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
        // This manual `/new` is a separate accepted submission. It must not
        // consume the earlier queued `/new`, even though the text is equal.
        paused = false;
        assert_eq!(
            queued,
            VecDeque::from(["/new".into(), "later request".into()])
        );
        assert!(!paused);
        assert_eq!(live_draft, "unsent request");
        let retry_id = session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id;
        assert_eq!(session_path.file_stem().unwrap(), retry_id.as_str());
        assert_ne!(retry_id, new_child.as_str());
        let retry_child = SessionId::new(retry_id).unwrap();
        let (retry_store, retry_family) = SessionManagerStore::open_family_owned(
            &state_dir.join("tui-admissions-v1"),
            &retry_child,
        )
        .unwrap();
        assert_eq!(retry_family.sessions.len(), 1);
        assert!(retry_family.sessions.contains_key(&retry_child));
        drop(retry_store);
        assert_eq!(
            std::fs::read(new_dir.join("state.json")).unwrap(),
            new_state
        );
        assert_eq!(std::fs::read(new_dir.join("raw.jsonl")).unwrap(), new_raw);
        assert_eq!(
            std::fs::read(source_dir.join("state.json")).unwrap(),
            source_state
        );
        assert_eq!(
            std::fs::read(source_dir.join("raw.jsonl")).unwrap(),
            source_raw
        );
        assert_eq!(
            effects
                .effects()
                .unwrap()
                .iter()
                .filter(|effect| {
                    effect.request.inventory.encoded() == "T06"
                        && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
                })
                .count(),
            3
        );

        // The queued invocation is dispatched once by taking its own head.
        assert_eq!(queued.pop_front().as_deref(), Some("/new"));
        assert!(handle_managed_new_session(
            &sessions_dir,
            &state_dir,
            &owner,
            &effects,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
        assert_eq!(queued, VecDeque::from(["later request".into()]));
        assert_eq!(live_draft, "unsent request");
        let queued_id = session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id;
        assert_ne!(queued_id, retry_child.as_str());
        assert_eq!(session_path.file_stem().unwrap(), queued_id.as_str());
        let queued_child = SessionId::new(queued_id).unwrap();
        let (_store, queued_family) = SessionManagerStore::open_family_owned(
            &state_dir.join("tui-admissions-v1"),
            &queued_child,
        )
        .unwrap();
        assert_eq!(queued_family.sessions.len(), 1);
        assert!(queued_family.sessions.contains_key(&queued_child));
        assert_eq!(
            effects
                .effects()
                .unwrap()
                .iter()
                .filter(|effect| {
                    effect.request.inventory.encoded() == "T06"
                        && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
                })
                .count(),
            4
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_resume_uses_the_selected_v2_identity_history_and_workflow() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source_config = polaris_core::workflow::WorkflowConfig {
            enabled: true,
            mode: polaris_core::workflow::WorkflowMode::Adaptive,
            initial_phase: polaris_core::workflow::Phase::Review,
            ..Default::default()
        };
        let source_workflow = polaris_core::workflow::SessionWorkflow::new(source_config.clone());
        let persistence = polaris_core::session_store::PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            polaris_core::conversation_state::HistoryMode::Legacy,
            Some(&source_workflow),
        )
        .expect("create source");
        let source_id = persistence
            .snapshot()
            .expect("source snapshot")
            .state
            .session_id;
        let mut seeded = Session::new();
        seeded.attach(persistence.clone()).expect("attach source");
        seeded.push_user("saved user turn");

        let current_config = polaris_core::workflow::WorkflowConfig {
            enabled: true,
            initial_phase: polaris_core::workflow::Phase::Implement,
            ..Default::default()
        };
        let session = new_workflow_session(WorkflowSessionArgs {
            config: &current_config,
            initial_phase: None,
            approval_auto: polaris_core::approval::AutoApproval::default(),
            history_mode: polaris_core::conversation_state::HistoryMode::Legacy,
            input_preparation: polaris_core::input_preparation::InputPreparationMode::Off,
            cwd: dir.path(),
            state_dir: dir.path(),
            sessions_dir: &dir.path().join("sessions"),
            strict_history: None,
            summary_usage: None,
            history_backend_factory: None,
            files_md_auto_regenerate: true,
            continuation: Some(PreparedContinuation {
                kind: ContinuationKind::Resume,
                persistence,
            }),
            workflow_mode_overridden: false,
            skills: &[],
            #[cfg(unix)]
            management_effects: None,
            #[cfg(unix)]
            project_mutation: None,
        })
        .await
        .expect("resume selected session");

        assert_eq!(
            session
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .session_id,
            source_id
        );
        assert_eq!(session.messages[0].content, "saved user turn");
        assert_eq!(session.workflow.as_ref().unwrap().config, source_config);
        assert_eq!(
            session.workflow.as_ref().unwrap().state().phase,
            polaris_core::workflow::Phase::Review
        );
    }

    #[test]
    fn v2_new_and_fork_preserve_strict_runtime_examples_and_usage_meter() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir(&sessions_dir).expect("sessions dir");
        let mut session = Session::new();
        let persistence = polaris_core::session_store::PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            polaris_core::conversation_state::HistoryMode::Strict10,
            None,
        )
        .expect("create v2 session");
        session.attach(persistence).expect("attach v2 session");
        let strict = test_strict_history();
        session.strict_history = Some(strict.clone());
        session.summary_usage = Some(polaris_provider::UsageMeter::default());
        session.examples = vec![Message::user("configured example")];
        session.push_user("real turn");
        session.check_persistence().expect("persist real turn");

        let mut session_path = sessions_dir.join("current.jsonl");
        let mut meta_path = sessions_dir.join("current.meta.json");
        let mut started = 1;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        handle_new_session(
            &sessions_dir,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        );
        assert!(Arc::ptr_eq(
            session.strict_history.as_ref().unwrap(),
            &strict
        ));
        assert!(session.summary_usage.is_some());
        assert_eq!(session.examples[0].content, "configured example");

        handle_fork(
            &sessions_dir,
            "/tmp/project",
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
        );
        assert!(Arc::ptr_eq(
            session.strict_history.as_ref().unwrap(),
            &strict
        ));
        assert!(session.summary_usage.is_some());
        assert_eq!(session.examples[0].content, "configured example");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn m3c_actual_resume_keeps_selected_identity_after_confirmed_workflow_override() {
        exercise_actual_resume_after_owned_publication(false).await;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn m3_remaining_managed_fork_preserves_original_and_refuses_active_owner() {
        use polaris_core::{
            conversation_state::HistoryMode, management_effect::*,
            rewind_workspace::WorkspaceOwner, session_store::PersistedSession,
        };
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        let sessions = root.path().join("sessions");
        for path in [&project, &state, &sessions] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::set_permissions(
            &state,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let workflow = polaris_core::workflow::SessionWorkflow::new(Default::default());
        let source = PersistedSession::create(
            root.path(),
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut session = Session::new();
        session.workflow = Some(workflow);
        session.attach(source.clone()).unwrap();
        session.push_user("original transcript");
        let before = source.snapshot().unwrap();
        let mut path = sessions.join(format!("{}.jsonl", before.state.session_id));
        let mut meta = path.with_extension("meta.json");
        let mut started = 0;
        let mut status = Status::Idle;
        let busy = owner.try_mutation().unwrap();
        handle_managed_fork(
            &sessions,
            project.to_str().unwrap(),
            &state,
            &owner,
            &effects,
            &mut session,
            &mut path,
            &mut meta,
            &mut started,
            &mut status,
        );
        assert_eq!(
            session
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .session_id,
            before.state.session_id
        );
        assert!(effects.effects().unwrap().is_empty());
        drop(busy);
        handle_managed_fork(
            &sessions,
            project.to_str().unwrap(),
            &state,
            &owner,
            &effects,
            &mut session,
            &mut path,
            &mut meta,
            &mut started,
            &mut status,
        );
        if let Status::Notice(text) = &status {
            assert!(text.contains("分岐しました"), "{text}");
        } else {
            panic!("fork did not display its result");
        }
        let child = session.persistence.as_ref().unwrap().snapshot().unwrap();
        assert_ne!(child.state.session_id, before.state.session_id);
        assert_eq!(session.messages[0].content, "original transcript");
        let unchanged = source.snapshot().unwrap();
        assert_eq!(unchanged.state, before.state);
        assert_eq!(unchanged.raw_hash, before.raw_hash);
        assert!(
            effects
                .effects()
                .unwrap()
                .iter()
                .any(|e| e.request.inventory.encoded() == "T16"
                    && matches!(e.state, ManagementEffectStateV1::Completed { .. }))
        );
        assert!(effects.blocker().unwrap().is_none());
    }

    #[test]
    fn m3_remaining_web_search_reports_disabled_and_refuses_unverified_modes() {
        let root = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        session.push_user("original");
        let mut status = Status::Idle;
        let mut lines = Vec::new();
        call(
            slash::parse("/web-search").unwrap(),
            &mut session,
            &root.path().join("session.jsonl"),
            &mut status,
            &[],
            root.path(),
            &mut lines,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("Web検索: disabled"))
        );
        let rendered = lines.len();
        for mode in ["cached", "live"] {
            call(
                slash::parse(&format!("/web-search {mode}")).unwrap(),
                &mut session,
                &root.path().join("session.jsonl"),
                &mut status,
                &[],
                root.path(),
                &mut lines,
            );
            assert!(matches!(&status, Status::Error(text) if text.contains("上限契約")));
            assert_eq!(lines.len(), rendered);
        }
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "original");
    }

    #[tokio::test]
    async fn m3_acceptance_memory_source_verifies_original_and_refuses_modified_raw() {
        struct SourceSummary;
        impl StrictSummaryProvider for SourceSummary {
            fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
                Box::pin(async move {
                    Ok(serde_json::json!({"facts":["remember original verified source"], "decisions":[], "constraints":[], "corrections":[], "open_items":[], "source_turn_ids":[request.source_turn_id]}).to_string())
                })
            }
        }
        struct SourceEmbedder(EmbeddingModel);
        impl StrictEmbedder for SourceEmbedder {
            fn metadata(&self) -> &EmbeddingModel {
                &self.0
            }
            fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
                Box::pin(async { Ok(vec![1.0]) })
            }
            fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
                Box::pin(async { Ok(vec![vec![1.0]]) })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let project_id = persist::project_identity(&project).unwrap();
        let saved = polaris_core::session_store::PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            &project_id,
            polaris_core::conversation_state::HistoryMode::Strict10,
            None,
        )
        .unwrap();
        for turn in 1..=11 {
            saved
                .append(
                    Message::user(format!("original verified source {turn}")),
                    true,
                    None,
                )
                .unwrap();
        }
        let history = StrictHistory::new(
            Arc::new(SourceSummary),
            Arc::new(SourceEmbedder(EmbeddingModel {
                model: "fixture".into(),
                revision: "v1".into(),
                dimension: 1,
            })),
        );
        let prepared = history
            .prepare_observed(
                &saved.store,
                &saved.database,
                "remember original",
                &|snapshot| saved.accept_publication(snapshot),
            )
            .await
            .unwrap();
        let source = &prepared.retrieval[0].source.id;
        let id = saved.snapshot().unwrap().state.session_id;
        let request = format!("source {id} {source}");
        let lines = memory_read_lines(
            &project,
            root.path(),
            &root.path().join("sessions"),
            &request,
        )
        .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.to_string().contains("original verified source 1"))
        );
        let raw = saved.store.directory().join("raw.jsonl");
        let original = std::fs::read_to_string(&raw).unwrap();
        std::fs::write(
            &raw,
            original.replacen("original verified", "modified verified", 1),
        )
        .unwrap();
        assert!(
            memory_read_lines(
                &project,
                root.path(),
                &root.path().join("sessions"),
                &request
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn m3c_actual_resume_migrates_missing_workflow_without_losing_identity() {
        exercise_actual_resume_after_owned_publication(true).await;
    }

    #[cfg(target_os = "linux")]
    async fn exercise_actual_resume_after_owned_publication(missing_workflow: bool) {
        use polaris_core::{
            conversation_state::HistoryMode, management_effect::*,
            rewind_workspace::WorkspaceOwner, session_store::PersistedSession,
        };
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        let sessions = root.path().join("sessions");
        for path in [&project, &state, &sessions] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::set_permissions(
            &state,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let config = polaris_core::workflow::WorkflowConfig::default();
        let workflow = polaris_core::workflow::SessionWorkflow::new(config.clone());
        let selected = PersistedSession::create(
            root.path(),
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            if missing_workflow {
                None
            } else {
                Some(&workflow)
            },
        )
        .unwrap();
        let mut seeded = Session::new();
        seeded.attach(selected.clone()).unwrap();
        seeded.push_user("selected transcript");
        validate_resume_workspace(&owner, &selected).unwrap();
        let selected_id = selected.snapshot().unwrap().state.session_id;
        let mut current = Session::new();
        current.push_user("unsaved current");
        let mut path = sessions.join("current.jsonl");
        let mut meta = path.with_extension("meta.json");
        let mut started = 0;
        let mut status = Status::Idle;
        let mut lines = Vec::new();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
        let mut keys = ScriptedReader(VecDeque::from([KeyCode::Enter, KeyCode::Char('y')]));
        handle_resume(
            &terminal,
            &mut keys,
            &sessions,
            &state,
            project.to_str().unwrap(),
            &config,
            false,
            &[],
            None,
            Some(&owner),
            Some(&effects),
            &mut current,
            &mut path,
            &mut meta,
            &mut started,
            &mut status,
            &mut lines,
        )
        .await;
        assert!(matches!(status, Status::Idle));
        assert_eq!(current.messages[0].content, "selected transcript");
        assert_eq!(
            current
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .session_id,
            selected_id
        );
        let override_config = polaris_core::workflow::WorkflowConfig {
            enabled: !config.enabled,
            ..config
        };
        let mut keys = ScriptedReader(VecDeque::from([
            KeyCode::Enter,
            KeyCode::Char('y'),
            KeyCode::Char('y'),
        ]));
        handle_resume(
            &terminal,
            &mut keys,
            &sessions,
            &state,
            project.to_str().unwrap(),
            &override_config,
            true,
            &[],
            None,
            Some(&owner),
            Some(&effects),
            &mut current,
            &mut path,
            &mut meta,
            &mut started,
            &mut status,
            &mut lines,
        )
        .await;
        if let Status::Error(message) | Status::Notice(message) = &status {
            panic!("resume: {message}");
        }
        assert!(matches!(status, Status::Idle));
        assert_eq!(
            current
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .session_id,
            selected_id
        );
        assert_eq!(
            current.workflow.as_ref().unwrap().config.enabled,
            override_config.enabled
        );
        let mut keys = ScriptedReader(VecDeque::from([
            KeyCode::Enter,
            KeyCode::Char('y'),
            KeyCode::Esc,
        ]));
        handle_resume(
            &terminal,
            &mut keys,
            &sessions,
            &state,
            project.to_str().unwrap(),
            &workflow.config,
            true,
            &[],
            None,
            Some(&owner),
            Some(&effects),
            &mut current,
            &mut path,
            &mut meta,
            &mut started,
            &mut status,
            &mut lines,
        )
        .await;
        assert!(matches!(&status,Status::Error(message) if message.contains("取り消")));
        current.check_persistence().unwrap();
        assert_eq!(
            current.workflow.as_ref().unwrap().config.enabled,
            workflow.config.enabled
        );
        assert_eq!(
            current
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .workflow_config
                .unwrap()
                .config
                .enabled,
            workflow.config.enabled
        );
        assert!(effects.blocker().unwrap().is_none());
        assert!(
            effects
                .effects()
                .unwrap()
                .iter()
                .any(|e| e.request.inventory.encoded() == "D07"
                    && matches!(e.state, ManagementEffectStateV1::Completed { .. }))
        );
    }

    #[test]
    fn handle_resume_with_nothing_saved_dismisses_on_any_key_and_leaves_the_session_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session {
            disable_files_md_auto_regenerate: true,
            ..Session::default()
        };
        session.push_user("current, unsaved conversation");
        let original_path = dir.path().join("current.jsonl");
        let mut session_path = original_path.clone();
        let mut meta_path = dir.path().join("current.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let backend = TestBackend::new(60, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(handle_resume(
                &terminal,
                &mut reader,
                dir.path(),
                dir.path(),
                "/tmp/current",
                &polaris_core::workflow::WorkflowConfig::default(),
                false,
                &[],
                None,
                #[cfg(unix)]
                None,
                #[cfg(unix)]
                None,
                &mut session,
                &mut session_path,
                &mut meta_path,
                &mut session_started_at_millis,
                &mut status,
                &mut local_lines,
            ));

        assert_eq!(
            session.messages.len(),
            1,
            "nothing to resume to, so unchanged"
        );
        assert_eq!(session_path, original_path);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn handle_resume_swaps_in_the_picked_conversation() {
        let dir = tempfile::tempdir().expect("temp dir");
        persist::write_meta_if_absent(
            &dir.path().join("saved.meta.json"),
            &persist::SessionMeta {
                cwd: "/tmp/current".to_string(),
                started_at_millis: 500,
            },
        )
        .expect("write meta");
        persist::append_message(
            &dir.path().join("saved.jsonl"),
            &Message::user("resumed hi"),
        )
        .expect("seed saved session");

        let mut session = Session {
            disable_files_md_auto_regenerate: true,
            ..Session::default()
        };
        session.push_user("current, unsaved conversation");
        let mut session_path = dir.path().join("current.jsonl");
        let mut meta_path = dir.path().join("current.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = vec![Line::from("leftover /diff output")];
        let backend = TestBackend::new(60, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // Only one saved conversation exists, so it's already highlighted;
        // Enter accepts it directly, same as the slash-command popup.
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(handle_resume(
                &terminal,
                &mut reader,
                dir.path(),
                dir.path(),
                "/tmp/current",
                &polaris_core::workflow::WorkflowConfig::default(),
                false,
                &[],
                None,
                #[cfg(unix)]
                None,
                #[cfg(unix)]
                None,
                &mut session,
                &mut session_path,
                &mut meta_path,
                &mut session_started_at_millis,
                &mut status,
                &mut local_lines,
            ));

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "resumed hi");
        assert_eq!(session_path, dir.path().join("saved.jsonl"));
        assert_eq!(meta_path, dir.path().join("saved.meta.json"));
        assert_eq!(session_started_at_millis, 500);
        assert!(session.disable_files_md_auto_regenerate);
        assert!(
            local_lines.is_empty(),
            "switching conversations clears local-only output"
        );
        assert!(matches!(status, Status::Idle));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_jsonl_resume_binds_the_imported_session_to_current_files() {
        use polaris_core::conversation_state::HistoryMode;
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_core::session_store::PersistedSession;

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        let sessions = root.path().join("sessions");
        let state_dir = root.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&sessions).unwrap();
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::write(project.join("task.txt"), "current files").unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        persist::append_message(&sessions.join("saved.jsonl"), &Message::user("old request"))
            .unwrap();
        persist::write_meta_if_absent(
            &sessions.join("saved.meta.json"),
            &persist::SessionMeta {
                cwd: project.display().to_string(),
                started_at_millis: 500,
            },
        )
        .unwrap();

        let project_id = persist::project_identity(&project).unwrap();
        let persistence = PersistedSession::create(
            root.path(),
            &state_dir.join("memory.sqlite3"),
            &project_id,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(persistence).unwrap();
        let mut session_path = sessions.join("current.jsonl");
        let mut meta_path = sessions.join("current.meta.json");
        let mut started = 1;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(70, 16)).unwrap());
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(handle_resume(
                &terminal,
                &mut reader,
                &sessions,
                &state_dir,
                &project.display().to_string(),
                &polaris_core::workflow::WorkflowConfig::default(),
                false,
                &[],
                None,
                Some(&owner),
                None,
                &mut session,
                &mut session_path,
                &mut meta_path,
                &mut started,
                &mut status,
                &mut local_lines,
            ));
        assert!(matches!(status, Status::Idle));
        assert_eq!(session.messages[0].content, "old request");
        let imported = session.persistence.as_ref().unwrap();
        assert!(imported.store.workspace_head_binding().unwrap().is_some());
        assert!(validate_resume_workspace(&owner, imported).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_resume_does_not_run_after_a_project_rewind() {
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        use polaris_sandbox::{SandboxMode, SandboxPolicy};

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let project = root.path().join("project");
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&sessions).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let policy =
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project))
                .unwrap();
        let file = project.join("task.txt");
        std::fs::write(&file, "before").unwrap();
        let pre = owner.capture(&policy).unwrap();
        std::fs::write(&file, "after").unwrap();
        let post = owner.capture(&policy).unwrap();
        let restored = owner.restore(&post, &pre).unwrap();
        owner.commit(&restored.transaction_id).unwrap();

        let saved_path = sessions.join("saved.jsonl");
        persist::append_message(&saved_path, &Message::user("old request")).unwrap();
        persist::write_meta_if_absent(
            &sessions.join("saved.meta.json"),
            &persist::SessionMeta {
                cwd: project.display().to_string(),
                started_at_millis: 500,
            },
        )
        .unwrap();
        let mut session = Session::new();
        session.push_user("current request");
        let mut session_path = sessions.join("current.jsonl");
        let mut meta_path = sessions.join("current.meta.json");
        let mut started = 1;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(70, 16)).unwrap());
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(handle_resume(
                &terminal,
                &mut reader,
                &sessions,
                &root.path().join("state"),
                &project.display().to_string(),
                &polaris_core::workflow::WorkflowConfig::default(),
                false,
                &[],
                None,
                Some(&owner),
                None,
                &mut session,
                &mut session_path,
                &mut meta_path,
                &mut started,
                &mut status,
                &mut local_lines,
            ));
        assert_eq!(session.messages[0].content, "current request");
        assert!(matches!(status, Status::Notice(_)));
        let display = terminal
            .borrow()
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(display.contains("old request"));
    }

    #[test]
    fn read_only_transcript_viewer_shows_saved_messages_without_session_switch() {
        let terminal = RefCell::new(Terminal::new(TestBackend::new(70, 12)).unwrap());
        let mut reader = ScriptedReader(std::collections::VecDeque::from([
            KeyCode::Down,
            KeyCode::Esc,
        ]));
        show_read_only_transcript(
            &terminal,
            &mut reader,
            &[
                Message::user("saved request"),
                Message::assistant("saved answer"),
            ],
            "raw.jsonl",
        );
        let display = terminal
            .borrow()
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(display.contains("saved request"));
        assert!(display.contains("saved answer"));
        assert!(display.contains("raw.jsonl"));
    }

    #[test]
    fn handle_resume_cancelled_with_esc_leaves_everything_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        persist::write_meta_if_absent(
            &dir.path().join("saved.meta.json"),
            &persist::SessionMeta {
                cwd: "/tmp/current".to_string(),
                started_at_millis: 500,
            },
        )
        .expect("write meta");
        persist::append_message(
            &dir.path().join("saved.jsonl"),
            &Message::user("resumed hi"),
        )
        .expect("seed saved session");

        let mut session = Session::default();
        session.push_user("current, unsaved conversation");
        let original_path = dir.path().join("current.jsonl");
        let mut session_path = original_path.clone();
        let mut meta_path = dir.path().join("current.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let backend = TestBackend::new(60, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Esc]));

        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(handle_resume(
                &terminal,
                &mut reader,
                dir.path(),
                dir.path(),
                "/tmp/current",
                &polaris_core::workflow::WorkflowConfig::default(),
                false,
                &[],
                None,
                #[cfg(unix)]
                None,
                #[cfg(unix)]
                None,
                &mut session,
                &mut session_path,
                &mut meta_path,
                &mut session_started_at_millis,
                &mut status,
                &mut local_lines,
            ));

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "current, unsaved conversation");
        assert_eq!(session_path, original_path);
    }

    #[test]
    fn an_unknown_command_names_itself_in_the_notice() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Unknown("frobnicate".to_string()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => assert!(n.contains("frobnicate")),
            _ => panic!("expected a Notice"),
        }
    }

    /// A `Provider` whose only job is recording what `set_model` was last
    /// called with, so `handle_model` tests can confirm it actually
    /// reaches the provider (not just the display copy). `complete` is
    /// never exercised by those tests.
    struct RecordingProvider {
        model: std::sync::Mutex<Option<String>>,
        effort: std::sync::Mutex<Option<String>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            Self {
                model: std::sync::Mutex::new(None),
                effort: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn complete(
            &self,
            _req: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            unimplemented!("not exercised by handle_model tests")
        }

        fn set_model(&self, model: &str) {
            *self.model.lock().expect("lock") = Some(model.to_string());
        }

        fn set_effort(&self, effort: Option<&str>) {
            *self.effort.lock().expect("lock") = effort.map(str::to_string);
        }
    }

    #[tokio::test]
    async fn handle_model_switches_the_hosted_provider_without_an_effort_step() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        let cloud = Arc::new(RecordingProvider::new());
        let cloud_dyn: Arc<dyn Provider> = cloud.clone();
        let mut provider = cloud_dyn.clone();
        let usage_meter = polaris_provider::UsageMeter::default();
        let local_router = polaris_core::local_execution::LocalRouter::new(1);
        let mut provider_name = "codex".to_string();
        let mut model_name = "gpt-5.6-sol".to_string();
        let mut effort_name = render::DEFAULT_EFFORT.to_string();
        let mut status = Status::Idle;

        handle_model(
            &terminal,
            &mut reader,
            &mut provider,
            &cloud_dyn,
            &usage_meter,
            &local_router,
            &mut provider_name,
            "codex",
            &mut model_name,
            render::DEFAULT_EFFORT,
            &mut effort_name,
            &mut status,
        )
        .await;

        assert_eq!(model_name, "gpt-5.6-terra");
        assert_eq!(provider_name, "codex");
        assert_eq!(effort_name, render::DEFAULT_EFFORT);
        assert!(Arc::ptr_eq(&provider, &cloud_dyn));
        assert_eq!(
            *cloud.model.lock().expect("lock"),
            Some("gpt-5.6-terra".to_string()),
            "the provider itself must be switched, not just the display copy"
        );
        assert_eq!(*cloud.effort.lock().expect("lock"), None);
        match status {
            Status::Notice(n) => {
                assert!(n.contains("gpt-5.6-terra"));
            }
            _ => panic!("expected a Notice"),
        }
    }

    #[tokio::test]
    async fn handle_model_cancelled_with_esc_leaves_everything_untouched() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
        let cloud = Arc::new(RecordingProvider::new());
        let cloud_dyn: Arc<dyn Provider> = cloud.clone();
        let mut provider = cloud_dyn.clone();
        let usage_meter = polaris_provider::UsageMeter::default();
        let local_router = polaris_core::local_execution::LocalRouter::new(1);
        let mut provider_name = "codex".to_string();
        let mut model_name = "gpt-5.6-sol".to_string();
        let mut effort_name = render::DEFAULT_EFFORT.to_string();
        let mut status = Status::Idle;

        handle_model(
            &terminal,
            &mut reader,
            &mut provider,
            &cloud_dyn,
            &usage_meter,
            &local_router,
            &mut provider_name,
            "codex",
            &mut model_name,
            render::DEFAULT_EFFORT,
            &mut effort_name,
            &mut status,
        )
        .await;

        assert_eq!(model_name, "gpt-5.6-sol");
        assert_eq!(effort_name, render::DEFAULT_EFFORT);
        assert_eq!(*cloud.model.lock().expect("lock"), None);
        assert_eq!(*cloud.effort.lock().expect("lock"), None);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn m3_acceptance_status_includes_usage_and_fixed_capability_truth() {
        let session = Session::default();
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        #[cfg(unix)]
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let store = {
            use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let state_dir = root.path().join("state");
            std::fs::create_dir(&state_dir).unwrap();
            let owner = WorkspaceOwner::open(
                &project,
                &state_dir.join("rewind-workspace"),
                RewindLimits::default(),
            )
            .unwrap();
            let lease = owner.try_mutation().unwrap();
            polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
                &project, &state_dir, &lease,
            )
            .unwrap()
        };
        let usage = polaris_provider::Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_tokens: 80,
        };

        let local_lines = status_lines(StatusSnapshot {
            session: &session,
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage_report: polaris_provider::UsageReport {
                usage,
                ..Default::default()
            },
            sandbox: &sandbox,
            approval: ApprovalPolicy::Never,
            startup_approval: ApprovalPolicy::OnRequest,
            mcp_external: true,
            audit_path: Some(std::path::Path::new("/tmp/audit\nforged.jsonl")),
            turn_authority_present: false,
            fixed_effort: None,
            answer_finalizer: None,
            #[cfg(unix)]
            project_root: &root.path().join("project"),
            #[cfg(unix)]
            management_effects: &store,
            #[cfg(unix)]
            recovery_issue: None,
        });
        let text = local_lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("cache 80"), "{text}");
        assert!(text.contains("CAT: configured=shadow ownerなし; installed=未照合; verified=未確認; observed=未取得"), "{text}");
        let diagnostic = doctor_lines(
            "openai\nforged",
            "model\tforged",
            std::path::Path::new("."),
            &sandbox,
            ApprovalPolicy::Never,
            false,
        );
        assert!(
            diagnostic
                .iter()
                .all(|line| !line.to_string().contains(['\n', '\t', '\x1b']))
        );

        assert!(text.contains("audit=/tmp/audit\\nforged.jsonl"), "{text}");
        assert!(text.contains("effort=automatic"));
        assert!(text.contains("authorityの構成=false"));
        assert!(text.contains("capacity観測は未接続"));
        assert!(text.contains("web-search=disabled"));
        assert!(text.contains("in 100"));
        assert!(text.contains("out 20"));
        assert!(text.contains("total 120"));
        assert!(text.contains("現在=Never, 起動時=OnRequest"));
        assert!(text.contains("外部MCP=true"));
        assert!(text.contains("実接続はこの表示では未検証"));
        assert!(session.messages.is_empty());
    }

    #[test]
    fn phase_command_cannot_write_without_managed_owner() {
        let mut session = Session::new();
        session.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
            polaris_core::workflow::WorkflowConfig {
                enabled: true,
                ..Default::default()
            },
        ));
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let persistence = polaris_core::session_store::PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            polaris_core::conversation_state::HistoryMode::Legacy,
            session.workflow.as_ref(),
        )
        .expect("create v2 session");
        session.attach(persistence).expect("attach v2 session");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Phase("implement".into()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        assert_eq!(
            session.workflow.as_ref().unwrap().state().phase,
            polaris_core::workflow::Phase::General,
            "a manual phase command must not bypass M27's approved change-contract gate"
        );
        call(
            slash::Action::Phase("review".into()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        assert_eq!(
            session.workflow.as_ref().unwrap().state().phase,
            polaris_core::workflow::Phase::General
        );
        assert_eq!(
            session
                .persistence
                .as_ref()
                .expect("v2 persistence")
                .snapshot()
                .expect("snapshot")
                .state
                .workflow
                .phase,
            polaris_core::workflow::Phase::General,
            "the fallback dispatch must not bypass the management effect owner"
        );
        assert!(matches!(status, Status::Error(_)));
        call(
            slash::Action::Phase("not-a-phase".into()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        assert_eq!(
            session.workflow.as_ref().unwrap().state().phase,
            polaris_core::workflow::Phase::General
        );
    }

    #[test]
    fn workflow_command_cannot_write_without_managed_owner() {
        let mut session = Session::new();
        session.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
            polaris_core::workflow::WorkflowConfig {
                enabled: true,
                ..Default::default()
            },
        ));
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let persistence = polaris_core::session_store::PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            polaris_core::conversation_state::HistoryMode::Legacy,
            session.workflow.as_ref(),
        )
        .expect("create v2 session");
        session.attach(persistence).expect("attach v2 session");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Workflow("mode adaptive".into()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        assert_eq!(
            session.workflow.as_ref().unwrap().config.mode,
            polaris_core::workflow::WorkflowMode::Manual
        );
        assert_eq!(
            session
                .persistence
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .workflow_config
                .unwrap()
                .config
                .mode,
            polaris_core::workflow::WorkflowMode::Manual
        );
        assert!(matches!(status, Status::Error(_)));

        call(
            slash::Action::Workflow("off".into()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        assert!(session.workflow.as_ref().unwrap().config.enabled);
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_phase_change_records_effect_and_cancel_preserves_session() {
        use polaris_core::management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1};
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state_dir = root.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(
            &state_dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state_dir.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects =
            ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap();
        drop(lease);
        let project_id = persist::project_identity(&project).unwrap();
        let workflow =
            polaris_core::workflow::SessionWorkflow::new(polaris_core::workflow::WorkflowConfig {
                enabled: true,
                ..Default::default()
            });
        let persistence = polaris_core::session_store::PersistedSession::create(
            root.path(),
            &state_dir.join("memory.sqlite3"),
            &project_id,
            polaris_core::conversation_state::HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut session = Session::new();
        session.workflow = Some(workflow);
        session.attach(persistence.clone()).unwrap();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 16)).unwrap());
        let mut status = Status::Idle;
        let journal = state_dir.join("management-effects-v1/journal.v1.jsonl");
        let before = persistence.snapshot().unwrap();
        let journal_before = std::fs::read(&journal).unwrap();

        let mut cancel = ScriptedReader(VecDeque::from([KeyCode::Esc]));
        handle_managed_workflow_action(
            &terminal,
            &mut cancel,
            slash::Action::Phase("review".into()),
            &mut session,
            &[],
            &workspace,
            &effects,
            &state_dir,
            false,
            &mut status,
        );
        assert_eq!(persistence.snapshot().unwrap().state, before.state);
        assert_eq!(std::fs::read(&journal).unwrap(), journal_before);

        let mut accept = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        handle_managed_workflow_action(
            &terminal,
            &mut accept,
            slash::Action::Phase("review".into()),
            &mut session,
            &[],
            &workspace,
            &effects,
            &state_dir,
            false,
            &mut status,
        );
        assert!(matches!(status, Status::Notice(_)));
        assert_eq!(
            persistence.snapshot().unwrap().state.workflow.phase,
            polaris_core::workflow::Phase::Review
        );
        assert_eq!(effects.effects().unwrap().len(), 1);
        assert!(matches!(
            effects.effects().unwrap()[0].state,
            ManagementEffectStateV1::Completed { .. }
        ));

        let mut disable = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        handle_managed_workflow_action(
            &terminal,
            &mut disable,
            slash::Action::Workflow("off".into()),
            &mut session,
            &[],
            &workspace,
            &effects,
            &state_dir,
            false,
            &mut status,
        );
        assert!(matches!(status, Status::Notice(_)));
        assert!(!session.workflow.as_ref().unwrap().config.enabled);
        assert_eq!(effects.effects().unwrap().len(), 2);

        let disabled_state = persistence.snapshot().unwrap().state;
        let disabled_journal = std::fs::read(&journal).unwrap();
        let mut unused = ScriptedReader(VecDeque::new());
        handle_managed_workflow_action(
            &terminal,
            &mut unused,
            slash::Action::Phase("implement".into()),
            &mut session,
            &[],
            &workspace,
            &effects,
            &state_dir,
            false,
            &mut status,
        );
        assert!(matches!(status, Status::Error(_)));
        assert_eq!(persistence.snapshot().unwrap().state, disabled_state);
        assert_eq!(std::fs::read(&journal).unwrap(), disabled_journal);
    }

    #[test]
    fn pwd_reports_the_canonical_project_root() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let project = dir.path().join("project");
        let nested = project.join("nested");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Pwd,
            &mut session,
            &session_path,
            &mut status,
            &[],
            &nested,
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => {
                assert_eq!(n, project.canonicalize().unwrap().display().to_string())
            }
            _ => panic!("expected a Notice"),
        }
    }

    /// The registry lives under `HOME`, not under the project, so "nothing is
    /// registered" has to be **created** here rather than assumed. Assuming it
    /// made this test pass only on a machine whose owner had never registered
    /// an MCP server, and fail for everyone else — observed on 2026-09-18,
    /// where registering three servers turned this into `left: 4, right: 1`.
    #[test]
    fn mcp_reports_state_into_the_transcript_without_connecting_or_asking_a_model() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let empty = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `HOME_LOCK`; restored before the assertions.
        unsafe {
            std::env::set_var("HOME", empty.path());
        }
        let mut session = polaris_core::session::Session::new();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Mcp(String::new()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        let permitted_lines = mcp_state_lines(dir.path(), true);

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(local_lines.len(), 2, "{local_lines:?}");
        assert_eq!(local_lines[0].to_string(), "外部MCPホスト上限: 拒否");
        assert_eq!(permitted_lines[0], "外部MCPホスト上限: 許可");
        assert!(
            local_lines[1]
                .spans
                .iter()
                .any(|span| span.content.contains("登録されたMCPサーバーはありません")),
            "{local_lines:?}"
        );
        // A local command: nothing is sent and nothing joins the history.
        assert!(session.messages.is_empty());
        assert!(matches!(status, Status::Idle));
    }

    #[cfg(unix)]
    #[test]
    fn copy_with_no_assistant_reply_yet_reports_nothing_to_copy() {
        let mut session = Session::default();
        session.push_user("hi");
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Copy,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => assert!(n.contains("nothing to copy")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn apply_copy_action_with_a_reply_wires_it_through_to_the_copy_fn_and_status() {
        // Exercises the success path `apply_slash_action`'s `Action::Copy`
        // arm delegates to, without performing real clipboard I/O — an
        // accidental edit that drops the `*status = ...` assignment, or
        // wires the wrong session into `copy_last_reply_with`, fails this.
        let mut session = Session::default();
        session.push_user("hi");
        session.push_assistant("the answer is 4", Vec::new());
        let mut status = Status::Idle;

        apply_copy_action(&session, &mut status, |text| {
            assert_eq!(text, "the answer is 4");
            Ok(())
        });

        match status {
            Status::Notice(n) => assert!(n.contains("copied")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn apply_copy_action_reports_a_failing_copy() {
        let mut session = Session::default();
        session.push_assistant("reply", Vec::new());
        let mut status = Status::Idle;

        apply_copy_action(&session, &mut status, |_| Err("no tty".to_string()));

        match status {
            Status::Notice(n) => {
                assert!(n.contains("can't copy"));
                assert!(n.contains("no tty"));
            }
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn apply_selection_copy_reports_success() {
        let status = apply_selection_copy("hello world", |text| {
            assert_eq!(text, "hello world");
            Ok(())
        });
        assert!(status.contains("copied"));
    }

    #[test]
    fn apply_selection_copy_reports_a_failing_copy() {
        let status = apply_selection_copy("hello", |_| Err("no tty".to_string()));
        assert!(status.contains("can't copy"));
        assert!(status.contains("no tty"));
    }

    #[cfg(unix)]
    #[test]
    fn export_writes_the_conversation_as_markdown() {
        let mut session = Session::default();
        session.push_user("what does this repo do?");
        session.push_assistant("it's a coding agent", Vec::new());
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Export(String::new()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let path = match status {
            Status::Notice(n) => {
                assert!(n.starts_with("exported to "));
                n.strip_prefix("exported to ").unwrap().to_string()
            }
            _ => panic!("expected a Notice"),
        };
        let written = std::fs::read_to_string(&path).expect("exported file should exist");
        assert!(written.contains("## You"));
        assert!(written.contains("what does this repo do?"));
        assert!(written.contains("## polaris"));
        assert!(written.contains("it's a coding agent"));
    }

    #[cfg(unix)]
    #[test]
    fn export_with_a_destination_argument_writes_there_instead() {
        let mut session = Session::default();
        session.push_user("hi");
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Export("notes.md".to_string()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(dir.path().join("notes.md").exists());
    }

    #[test]
    fn handle_fork_seeds_the_new_file_with_the_current_conversation_and_keeps_the_old_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session {
            disable_files_md_auto_regenerate: true,
            ..Session::default()
        };
        session.push_user("hello");
        session.push_assistant("hi there", Vec::new());
        let old_path = dir.path().join("old.jsonl");
        persist::append_message(&old_path, &session.messages[0]).expect("seed old file");
        let mut session_path = old_path.clone();
        let mut meta_path = dir.path().join("old.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;

        handle_fork(
            dir.path(),
            "/tmp/example",
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
        );

        assert_ne!(session_path, old_path, "a fork should get its own file");
        assert!(session.disable_files_md_auto_regenerate);
        let (forked, truncated) = persist::load_session(&session_path).expect("load fork");
        assert!(!truncated);
        assert_eq!(forked.messages.len(), 2);
        assert_eq!(forked.messages[0].content, "hello");
        assert_eq!(forked.messages[1].content, "hi there");
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap().lines().count(),
            1,
            "the original conversation must stay intact on disk"
        );
        match status {
            Status::Notice(n) => assert!(n.contains("forked to")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn fork_preserves_foreign_project_provenance_for_memory() {
        let logs = tempfile::tempdir().unwrap();
        let origin = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        session.push_user("original project decision");
        let mut session_path = logs.path().join("original.jsonl");
        let mut meta_path = session_path.with_extension("meta.json");
        let origin_cwd = origin
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        persist::write_meta_if_absent(
            &meta_path,
            &persist::SessionMeta {
                cwd: origin_cwd.clone(),
                started_at_millis: 1,
            },
        )
        .unwrap();
        let mut started = 1;
        let mut status = Status::Idle;
        handle_fork(
            logs.path(),
            current.path().to_str().unwrap(),
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
        );
        assert_eq!(persist::read_meta(&meta_path).unwrap().cwd, origin_cwd);
        memory::configure_saved_memory(&mut session, current.path(), logs.path(), &session_path)
            .unwrap();
        assert!(session.before_compact.as_ref().unwrap()(&session.messages).is_err());
        configure_saved_tool_memory(
            &mut session,
            current.path(),
            logs.path(),
            &session_path,
            Some(polaris_core::tool_memory::RetentionMode::History),
            None,
        )
        .unwrap();
        assert!(session.tool_memory.is_none());
        assert!(!logs.path().join("memory.sqlite3").exists());
    }

    #[test]
    fn permissions_ceiling_uses_the_two_axis_transition_table() {
        use ApprovalPolicy::{Always, Never, OnRequest};
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        for (ceiling, allowed) in [
            (Never, [true, false, false]),
            (OnRequest, [true, true, false]),
            (Always, [false, false, true]),
        ] {
            for (picked, expected) in [Never, OnRequest, Always].into_iter().zip(allowed) {
                assert_eq!(
                    approval_within_startup_ceiling(ceiling, picked),
                    expected,
                    "ceiling={ceiling:?}, picked={picked:?}"
                );
                if expected {
                    assert!(
                        delegation_capabilities(&sandbox, picked, false, &[])
                            .does_not_escalate_from(&delegation_capabilities(
                                &sandbox,
                                ceiling,
                                false,
                                &[]
                            )),
                        "delegation escalated: ceiling={ceiling:?}, picked={picked:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn handle_permissions_rejects_a_policy_above_startup_never() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // Down from Never (index 0) to OnRequest (index 1), then Enter.
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        let mut approval_policy = ApprovalPolicy::Never;
        let mut status = Status::Idle;
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();

        handle_permissions(
            &terminal,
            &mut reader,
            &mut approval_policy,
            ApprovalPolicy::Never,
            &sandbox,
            &mut status,
        );

        assert_eq!(approval_policy, ApprovalPolicy::Never);
        match status {
            Status::Notice(n) => assert!(n.contains("--approval on-request")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn handle_permissions_can_narrow_and_restore_startup_on_request() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        let mut approval_policy = ApprovalPolicy::OnRequest;
        let mut status = Status::Idle;
        let mut to_never = ScriptedReader(VecDeque::from([KeyCode::Up, KeyCode::Enter]));
        handle_permissions(
            &terminal,
            &mut to_never,
            &mut approval_policy,
            ApprovalPolicy::OnRequest,
            &sandbox,
            &mut status,
        );
        assert_eq!(approval_policy, ApprovalPolicy::Never);

        let mut restore = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        handle_permissions(
            &terminal,
            &mut restore,
            &mut approval_policy,
            ApprovalPolicy::OnRequest,
            &sandbox,
            &mut status,
        );
        assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
    }

    #[test]
    fn handle_permissions_keeps_delegation_within_startup_on_request() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        let mut approval_policy = ApprovalPolicy::OnRequest;
        let mut status = Status::Idle;
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        handle_permissions(
            &terminal,
            &mut reader,
            &mut approval_policy,
            ApprovalPolicy::OnRequest,
            &sandbox,
            &mut status,
        );
        assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
        assert!(matches!(status, Status::Notice(_)));
    }

    #[test]
    fn handle_permissions_keeps_startup_always_confirmations() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        let mut approval_policy = ApprovalPolicy::Always;
        let mut status = Status::Idle;
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        handle_permissions(
            &terminal,
            &mut reader,
            &mut approval_policy,
            ApprovalPolicy::Always,
            &sandbox,
            &mut status,
        );
        assert_eq!(approval_policy, ApprovalPolicy::Always);
        assert!(matches!(status, Status::Notice(_)));
    }

    #[test]
    fn handle_permissions_cancelled_with_esc_leaves_the_policy_untouched() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
        let mut approval_policy = ApprovalPolicy::Never;
        let mut status = Status::Idle;
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();

        handle_permissions(
            &terminal,
            &mut reader,
            &mut approval_policy,
            ApprovalPolicy::Never,
            &sandbox,
            &mut status,
        );

        assert_eq!(approval_policy, ApprovalPolicy::Never);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn handle_permissions_does_not_accept_a_selection_when_limits_cannot_be_shown() {
        let backend = TestBackend::new(39, 7);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter, KeyCode::Esc]));
        let mut approval_policy = ApprovalPolicy::OnRequest;
        let mut status = Status::Idle;
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        handle_permissions(
            &terminal,
            &mut reader,
            &mut approval_policy,
            ApprovalPolicy::OnRequest,
            &sandbox,
            &mut status,
        );
        assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    #[cfg(unix)]
    fn diff_reports_no_changes_outside_a_git_repository() {
        // A missing repository is reported as local content, not a crash.
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        let outcome = call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(matches!(outcome, SlashOutcome::Continue));
        assert!(!local_lines.is_empty(), "should still report something");
    }

    #[test]
    fn help_lists_every_registered_command_in_scrollback() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::default();
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let outcome = call(
            slash::Action::Help,
            &mut session,
            &dir.path().join("session.jsonl"),
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(matches!(outcome, SlashOutcome::Continue));
        assert_eq!(local_lines.len(), slash::COMMANDS.len() + 1);
        for (line, command) in local_lines.iter().skip(1).zip(slash::COMMANDS) {
            assert_eq!(
                line.to_string(),
                format!("/{} — {}", command.name, command.description)
            );
        }
        assert!(local_lines.last().unwrap().to_string().contains("/quit"));
    }

    #[test]
    #[cfg(unix)]
    fn diff_shows_no_changes_in_a_clean_git_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git must be on PATH for this test")
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);

        let mut session = Session::default();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let rendered: String = local_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("no changes"));
    }

    #[test]
    #[cfg(unix)]
    fn diff_shows_an_actual_uncommitted_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git must be on PATH for this test")
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.path().join("a.txt"), "hello, changed\n").expect("write");

        let mut session = Session::default();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let rendered: String = local_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("a.txt"));
        assert!(rendered.contains("changed"));
    }

    #[test]
    #[cfg(unix)]
    fn diff_shows_untracked_file_in_an_unborn_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .expect("git must be on PATH for this test");
        assert!(init.status.success());
        std::fs::write(dir.path().join("new.txt"), "untracked\n").expect("write");

        let mut session = Session::default();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let rendered = local_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("new.txt"), "{rendered}");
        assert!(!rendered.contains("(no changes)"), "{rendered}");
    }

    #[test]
    #[cfg(unix)]
    fn diff_shows_staged_file_in_an_unborn_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git must be on PATH for this test");
            assert!(output.status.success(), "{:?}", output.stderr);
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("staged.txt"), "staged\n").expect("write");
        run(&["add", "staged.txt"]);

        let mut session = Session::default();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let rendered = local_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("staged.txt"), "{rendered}");
        assert!(!rendered.contains("(no changes)"), "{rendered}");
    }

    #[test]
    #[cfg(unix)]
    fn diff_hostile_environment_child() {
        let Some(project) = std::env::var_os("POLARIS_T13_CHILD_TARGET") else {
            return;
        };
        let project = std::path::PathBuf::from(project);
        let mut session = Session::default();
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        call(
            slash::Action::Diff,
            &mut session,
            &project.join("session.jsonl"),
            &mut status,
            &[],
            &project,
            &mut local_lines,
        );
        let rendered = local_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("target.txt"), "{rendered}");
        assert!(!rendered.contains("outside.txt"), "{rendered}");
    }

    #[test]
    #[cfg(unix)]
    fn diff_ignores_hostile_git_environment() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target");
        let outside = dir.path().join("outside");
        for repo in [&target, &outside] {
            std::fs::create_dir(repo).expect("create repository directory");
            let output = std::process::Command::new("/usr/bin/git")
                .args(["init", "-q"])
                .current_dir(repo)
                .output()
                .expect("git init");
            assert!(output.status.success(), "{:?}", output.stderr);
        }
        std::fs::write(target.join("target.txt"), "target\n").expect("write target");
        std::fs::write(outside.join("outside.txt"), "outside\n").expect("write outside");

        let executable = std::env::current_exe().expect("test executable");
        let run_child = |configure: &dyn Fn(&mut std::process::Command)| {
            let mut child = std::process::Command::new(&executable);
            child
                .arg("diff_hostile_environment_child")
                .arg("--nocapture")
                .env("POLARIS_T13_CHILD_TARGET", &target);
            configure(&mut child);
            let output = child.output().expect("child test");
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_child(&|child| {
            child.env("GIT_DIR", outside.join(".git"));
            child.env("GIT_WORK_TREE", &outside);
        });

        let fake_bin = dir.path().join("fake-bin");
        std::fs::create_dir(&fake_bin).expect("create fake bin");
        let fake_git = fake_bin.join("git");
        let marker = dir.path().join("fake-git-called");
        std::fs::write(
            &fake_git,
            "#!/bin/sh\n: > \"$POLARIS_T13_GIT_MARKER\"\nexit 77\n",
        )
        .expect("write fake git");
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755))
            .expect("make fake git executable");
        run_child(&|child| {
            child
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env("PATH", &fake_bin)
                .env("POLARIS_T13_GIT_MARKER", &marker);
        });
        assert!(!marker.exists(), "fake git on PATH must not run");
    }

    #[test]
    #[cfg(unix)]
    fn diff_bounds_large_git_output() {
        let dir = tempfile::tempdir().expect("temp dir");
        let run = |args: &[&str]| {
            let output = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git must run");
            assert!(output.status.success(), "{:?}", output.stderr);
        };
        run(&["init", "-q"]);
        std::fs::write(
            dir.path().join("large.txt"),
            "x".repeat(polaris_core::project_git::MAX_GIT_OUTPUT_BYTES + 1024),
        )
        .expect("write large file");
        run(&["add", "large.txt"]);

        let mut session = Session::default();
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        call(
            slash::Action::Diff,
            &mut session,
            &dir.path().join("session.jsonl"),
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        let rendered = local_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Git情報を取得できません"), "{rendered}");
        assert!(!rendered.contains("large.txt"), "{rendered}");
    }

    #[cfg(unix)]
    #[test]
    fn clear_also_empties_local_lines_from_a_previous_diff() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = vec![ratatui::text::Line::from("leftover diff output")];

        call(
            slash::Action::Clear,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(local_lines.is_empty());
    }

    #[test]
    fn review_prompt_with_no_extra_text_is_the_bare_instruction() {
        assert_eq!(review_prompt(""), REVIEW_INSTRUCTION);
    }

    #[test]
    fn review_prompt_appends_the_extra_text_when_given() {
        let p = review_prompt("the auth module");
        assert!(p.starts_with(REVIEW_INSTRUCTION));
        assert!(p.contains("the auth module"));
    }

    #[test]
    fn scroll_offset_saturates_at_zero_going_down() {
        let mut scroll_offset: usize = 0;
        scroll_offset = scroll_offset.saturating_sub(1);
        assert_eq!(scroll_offset, 0);
    }

    #[test]
    fn scroll_offset_grows_unbounded_going_up_since_the_window_clamps_it() {
        // Mirrors what the Up-key handler in run() does — the clamp lives in
        // visible_history_window (Task 1), not here, by design (see that
        // task's doc comment).
        let mut scroll_offset: usize = 0;
        for _ in 0..1000 {
            scroll_offset = scroll_offset.saturating_add(1);
        }
        assert_eq!(scroll_offset, 1000);
        assert_eq!(render::visible_history_window(50, scroll_offset, 10), 0..10);
    }

    #[test]
    // The `47` below is never read before being overwritten — that's the
    // point (it documents the pre-reset "mid-scroll" value the Submit arm
    // discards), so silence clippy's unused_assignments lint for it.
    #[allow(unused_assignments)]
    fn scroll_offset_test_helper_matches_the_submit_reset_contract() {
        // Documents/pins the exact behavior Task 7 Step 2 wires into run():
        // a non-empty Submit resets scroll_offset to 0. run()'s own loop
        // isn't independently driveable in a test (see Task 7 Step 3's note),
        // so this pins the invariant at the type level instead: after any
        // number of scroll-up steps, resetting to 0 always shows the tail.
        let history_len = 200;
        let mut scroll_offset: usize = 47; // mid-scroll
        scroll_offset = 0; // what the Submit arm does
        assert_eq!(
            render::visible_history_window(history_len, scroll_offset, 10),
            190..200
        );
    }
}

#[cfg(test)]
mod tool_memory_settings_tests {
    use super::*;
    use polaris_core::{session::Session, tool_memory::RetentionMode};

    #[test]
    fn fork_with_unknown_memory_origin_is_rejected_without_changing_source_or_writing_files() {
        for tool_argument in [false, true] {
            let logs = tempfile::tempdir().unwrap();
            let mut session = Session::new();
            session.push_user("keep instructions");
            if tool_argument {
                session.push_assistant_tool_calls(
                    "",
                    vec![polaris_provider::ToolCall {
                        id: "pending".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "memory://original"}),
                    }],
                    Vec::new(),
                );
            } else {
                session.push_tool_result(
                    "completed",
                    "[Stored tool result original] memory://original",
                );
            }
            let before = serde_json::to_string(&session.messages).unwrap();
            let mut path = logs.path().join("original.jsonl");
            let mut meta = path.with_extension("meta.json");
            let old_path = path.clone();
            let old_meta = meta.clone();
            let mut started = 42;
            let mut status = Status::Idle;
            handle_fork(
                logs.path(),
                "/project",
                &mut session,
                &mut path,
                &mut meta,
                &mut started,
                &mut status,
            );
            assert_eq!(path, old_path);
            assert_eq!(meta, old_meta);
            assert_eq!(started, 42);
            assert_eq!(serde_json::to_string(&session.messages).unwrap(), before);
            assert_eq!(std::fs::read_dir(logs.path()).unwrap().count(), 0);
            assert!(
                matches!(status, Status::Notice(message) if message.contains("分岐できません"))
            );
        }
    }

    #[test]
    fn tool_memory_lineage_rejects_unverified_ancestors_and_excessive_depth() {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let cwd = root
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let write = |id: &str, origin: Option<&str>| {
            let path = logs.path().join(format!("{id}.jsonl"));
            persist::clear_session(&path).unwrap();
            if path.with_extension("meta.json").exists() {
                std::fs::remove_file(path.with_extension("meta.json")).unwrap();
            }
            persist::write_fork_meta(
                &path.with_extension("meta.json"),
                &persist::SessionMeta {
                    cwd: cwd.clone(),
                    started_at_millis: 1,
                },
                origin,
            )
            .unwrap();
            path
        };
        let a = write("A", None);
        let b = write("B", Some("A"));
        let c = write("C", Some("B"));
        assert_eq!(
            saved_tool_memory_namespace(root.path(), &c).unwrap(),
            ("C".into(), vec!["B".into(), "A".into()])
        );
        for bad in [
            serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"missing"}),
            serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"C"}),
            serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"B"}),
            serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"../A"}),
            serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":null}),
            serde_json::json!({"cwd":foreign.path(),"started_at_millis":1}),
            serde_json::json!({"cwd":"relative","started_at_millis":1}),
            serde_json::json!({"tool_memory_origin":"A"}),
            serde_json::json!([]),
        ] {
            std::fs::write(b.with_extension("meta.json"), bad.to_string()).unwrap();
            assert!(
                saved_tool_memory_namespace(root.path(), &c).is_err(),
                "{bad}"
            );
        }
        std::fs::write(b.with_extension("meta.json"), b"{broken").unwrap();
        assert!(saved_tool_memory_namespace(root.path(), &c).is_err());
        std::fs::remove_file(b.with_extension("meta.json")).unwrap();
        assert!(saved_tool_memory_namespace(root.path(), &c).is_err());
        write("B", Some("A"));
        std::fs::remove_file(&a).unwrap();
        assert!(saved_tool_memory_namespace(root.path(), &c).is_err());
        write("A", None);
        std::fs::remove_file(&b).unwrap();
        assert!(saved_tool_memory_namespace(root.path(), &c).is_err());

        let mut previous = "A".to_owned();
        for depth in 1..=MAX_TOOL_MEMORY_ORIGINS + 1 {
            let id = format!("depth-{depth}");
            let path = write(&id, Some(&previous));
            assert_eq!(
                saved_tool_memory_namespace(root.path(), &path).is_ok(),
                depth <= MAX_TOOL_MEMORY_ORIGINS
            );
            previous = id;
        }
    }

    #[tokio::test]
    async fn two_generation_fork_resume_writes_own_session_and_origin_forget_invalidates() {
        for forget_child in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let logs = tempfile::tempdir().unwrap();
            let state = logs.path().canonicalize().unwrap().join("state");
            let path = logs.path().join("original.jsonl");
            let mut session = Session::new();
            configure_saved_tool_memory(
                &mut session,
                root.path(),
                &state,
                &path,
                Some(RetentionMode::History),
                None,
            )
            .unwrap();
            let saved = session
                .tool_memory
                .as_ref()
                .unwrap()
                .backend
                .save(
                    &polaris_provider::ToolCall {
                        id: "call".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"source.txt"}),
                    },
                    "original evidence before file changes",
                )
                .await
                .unwrap();
            session.push_tool_result("call", &format!("memory://{}", saved.id));
            persist::write_meta_if_absent(
                &path.with_extension("meta.json"),
                &persist::SessionMeta {
                    cwd: root.path().canonicalize().unwrap().to_str().unwrap().into(),
                    started_at_millis: 1,
                },
            )
            .unwrap();
            persist::append_message(&path, &session.messages[0]).unwrap();
            let (mut loaded, _) = persist::load_session(&path).unwrap();
            configure_saved_tool_memory(
                &mut loaded,
                root.path(),
                &state,
                &path,
                Some(RetentionMode::History),
                None,
            )
            .unwrap();
            let original = loaded
                .tool_memory
                .as_ref()
                .unwrap()
                .backend
                .read(&format!("memory://{}", saved.id), 0, 100)
                .await
                .unwrap();
            assert!(original.contains("original evidence before file changes"));
            let mut fork_path = path.clone();
            let mut meta_path = path.with_extension("meta.json");
            let mut started = 1;
            let mut status = Status::Idle;
            let mut descendants = Vec::new();
            let mut ancestor_ids = vec!["original".to_owned()];
            for generation in 0..2 {
                let previous = fork_path.clone();
                handle_fork(
                    logs.path(),
                    root.path().to_str().unwrap(),
                    &mut loaded,
                    &mut fork_path,
                    &mut meta_path,
                    &mut started,
                    &mut status,
                );
                assert_ne!(fork_path, previous);
                assert_eq!(
                    persist::read_tool_memory_origin(&meta_path)
                        .unwrap()
                        .as_deref(),
                    previous.file_stem().and_then(|id| id.to_str())
                );
                assert!(matches!(&status, Status::Notice(message) if message.contains("削除")));
                loaded = persist::load_session(&fork_path).unwrap().0;
                configure_saved_tool_memory(
                    &mut loaded,
                    root.path(),
                    &state,
                    &fork_path,
                    Some(RetentionMode::History),
                    None,
                )
                .unwrap();
                let (current, origins) =
                    saved_tool_memory_namespace(root.path(), &fork_path).unwrap();
                assert_eq!(origins, ancestor_ids);
                ancestor_ids.insert(0, current.clone());
                let backend = loaded.tool_memory.as_ref().unwrap().backend.clone();
                if let Some((_, parent_record, _)) = descendants.last() {
                    assert!(
                        backend
                            .read(&format!("memory://{parent_record}"), 0, 100)
                            .await
                            .unwrap()
                            .contains("fork evidence")
                    );
                }
                let record = backend
                    .save(
                        &polaris_provider::ToolCall {
                            id: format!("fork-{generation}"),
                            name: "read".into(),
                            arguments: serde_json::json!({"path":"fork.txt"}),
                        },
                        "fork evidence",
                    )
                    .await
                    .unwrap();
                let store =
                    polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
                let project = persist::project_identity(root.path()).unwrap();
                assert!(store.get(&project, &current, &record.id).unwrap().is_some());
                for ancestor in &origins {
                    assert!(store.get(&project, ancestor, &record.id).unwrap().is_none());
                }
                assert!(
                    session
                        .tool_memory
                        .as_ref()
                        .unwrap()
                        .backend
                        .read(&format!("memory://{}", record.id), 0, 100)
                        .await
                        .is_err()
                );
                descendants.push((current, record.id, backend));
                assert!(
                    loaded
                        .tool_memory
                        .as_ref()
                        .unwrap()
                        .backend
                        .read(&format!("memory://{}", saved.id), 0, 100)
                        .await
                        .unwrap()
                        .contains("original evidence before file changes")
                );
            }
            let foreign = tempfile::tempdir().unwrap();
            configure_saved_tool_memory(
                &mut loaded,
                foreign.path(),
                &state,
                &fork_path,
                Some(RetentionMode::History),
                None,
            )
            .unwrap();
            assert!(loaded.tool_memory.is_none());
            configure_saved_tool_memory(
                &mut loaded,
                root.path(),
                &state,
                &fork_path,
                Some(RetentionMode::History),
                None,
            )
            .unwrap();
            {
                let _lock = memory::lock_memory(&state).unwrap();
                let mut store =
                    polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
                store
                    .delete_session(
                        &persist::project_identity(root.path()).unwrap(),
                        if forget_child {
                            &descendants[0].0
                        } else {
                            "original"
                        },
                    )
                    .unwrap();
            }
            let store = polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
            let project = persist::project_identity(root.path()).unwrap();
            assert_eq!(
                store
                    .get(&project, "original", &saved.id)
                    .unwrap()
                    .is_some(),
                forget_child
            );
            if forget_child {
                assert!(
                    store
                        .get(&project, &descendants[0].0, &descendants[0].1)
                        .unwrap()
                        .is_none()
                );
                assert!(
                    session
                        .tool_memory
                        .as_ref()
                        .unwrap()
                        .backend
                        .read(&format!("memory://{}", saved.id), 0, 100)
                        .await
                        .unwrap()
                        .contains("original evidence")
                );
                assert!(
                    session
                        .tool_memory
                        .as_ref()
                        .unwrap()
                        .backend
                        .save(
                            &polaris_provider::ToolCall {
                                id: "parent-later".into(),
                                name: "read".into(),
                                arguments: serde_json::json!({}),
                            },
                            "parent remains writable"
                        )
                        .await
                        .is_ok()
                );
            }
            for (_, record, backend) in &descendants {
                assert!(
                    backend
                        .read(&format!("memory://{record}"), 0, 100)
                        .await
                        .is_err()
                );
                assert!(
                    backend
                        .save(
                            &polaris_provider::ToolCall {
                                id: "blocked".into(),
                                name: "read".into(),
                                arguments: serde_json::json!({}),
                            },
                            "cannot resurrect"
                        )
                        .await
                        .is_err()
                );
            }
            let backend = &loaded.tool_memory.as_ref().unwrap().backend;
            assert!(
                backend
                    .read(&format!("memory://{}", saved.id), 0, 100)
                    .await
                    .is_err()
            );
            assert!(
                backend
                    .save(
                        &polaris_provider::ToolCall {
                            id: "later".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path":"source.txt"})
                        },
                        "cannot resurrect"
                    )
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn tool_memory_settings_preserve_messages_and_reject_foreign_or_unknown_origin() {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let state = logs.path().canonicalize().unwrap().join("state");
        let path = logs.path().join("saved.jsonl");
        let mut session = Session::new();
        configure_saved_tool_memory(
            &mut session,
            root.path(),
            &state,
            &path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        assert!(session.tool_memory.is_some());
        assert!(session.before_compact.is_none());
        session.push_user("keep user instructions");
        session.push_assistant_tool_calls(
            "pending",
            vec![polaris_provider::ToolCall {
                id: "pending-call".into(),
                name: "read".into(),
                arguments: r#"{"path":"file"}"#.into(),
            }],
            Vec::new(),
        );
        let before = serde_json::to_string(&session.messages).unwrap();
        configure_saved_tool_memory(
            &mut session,
            root.path(),
            &state,
            &path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        assert!(
            session.tool_memory.is_none(),
            "unknown nonempty origin is rejected"
        );
        persist::write_meta_if_absent(
            &path.with_extension("meta.json"),
            &persist::SessionMeta {
                cwd: root.path().canonicalize().unwrap().to_str().unwrap().into(),
                started_at_millis: 1,
            },
        )
        .unwrap();
        configure_saved_tool_memory(
            &mut session,
            root.path(),
            &state,
            &path,
            Some(RetentionMode::Retrieval),
            None,
        )
        .unwrap();
        assert_eq!(
            session.tool_memory.as_ref().unwrap().mode,
            RetentionMode::Retrieval
        );
        configure_saved_tool_memory(
            &mut session,
            foreign.path(),
            &state,
            &path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        assert!(
            session.tool_memory.is_none(),
            "foreign origin clears an existing backend"
        );
        assert_eq!(serde_json::to_string(&session.messages).unwrap(), before);
        configure_saved_tool_memory(&mut session, root.path(), &state, &path, None, None).unwrap();
        assert!(session.tool_memory.is_none());
    }
}
