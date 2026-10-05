//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

pub mod approver;
mod clipboard;
#[cfg(test)]
mod continuation_tests;
pub mod input;
#[cfg(test)]
mod input_preparation_tests;
mod memory;
pub mod onboarding;
pub mod persist;
pub use memory::{configure_archive, configure_memory, lock_memory};
pub mod render;
mod selection;
pub mod sessions;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub mod signal;
pub mod slash;
mod time;
pub mod tool_memory;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
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

use approver::{CrosstermKeyReader, TuiApprover};
use input::{InputAction, apply_key};
use render::Status;

const UNDECIDED_EFFORT: &str = "undecided";
#[cfg(target_os = "linux")]
const LINUX_SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
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

/// CLI-owned constructor, invoked only when a selected strict session needs it.
pub type HistoryBackend = (
    Arc<polaris_core::conversation_memory::StrictHistory>,
    polaris_provider::UsageMeter,
);
pub type HistoryBackendFactory = Arc<dyn Fn() -> Result<HistoryBackend, String> + Send + Sync>;
pub type TurnAuthorityInstaller =
    Arc<dyn Fn(&mut polaris_core::session::Session, &str) -> Result<(), String> + Send + Sync>;

/// Everything `run()` needs, already built by `polaris-cli::main()` the
/// same way the one-shot path builds it. `polaris-tui` never constructs a
/// provider or a sandbox policy itself.
pub struct RunArgs<'a> {
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
    if needs_checkpoint || workflow_mode_overridden {
        continuation
            .persistence
            .checkpoint(Some(&workflow))
            .map_err(|error| format!("workflow設定を保存できません: {error}"))?;
    }
    candidate_session.workflow = Some(workflow);
    *session = candidate_session;
    Ok(())
}

async fn new_workflow_session(
    args: WorkflowSessionArgs<'_>,
) -> Result<polaris_core::session::Session, String> {
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
    } = args;
    let mut session = polaris_core::session::Session::new();
    session.input_preparation = input_preparation;
    session.disable_files_md_auto_regenerate = !files_md_auto_regenerate;
    session.approval_auto = approval_auto;
    session.strict_history = strict_history;
    session.summary_usage = summary_usage;
    let has_continuation = continuation.is_some();
    if let Some(continuation) = continuation {
        restore_prepared_continuation(
            &mut session,
            &continuation,
            config,
            workflow_mode_overridden,
            skills,
            history_backend_factory,
        )
        .await?;
    } else if config.enabled {
        let workflow = polaris_core::workflow::SessionWorkflow::try_new(config.clone())?;
        if let Some(phase) = initial_phase {
            workflow.request_phase(phase)?;
        }
        session.workflow = Some(workflow);
    } else if initial_phase.is_some() {
        return Err("--phase には設定で [workflow].enabled = true が必要です".into());
    }
    // Explicit workflow opt-out with legacy history retains the JSONL lifecycle.
    // Workflow state needs an atomic marker even when strict history is off.
    if !has_continuation
        && (config.enabled || history_mode != polaris_core::conversation_state::HistoryMode::Legacy)
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

#[derive(Debug)]
enum TuiAdmissionFailure {
    Retryable(String),
    Consumed(String),
}

impl TuiAdmissionFailure {
    fn consumed(message: impl Into<String>) -> Self {
        Self::Consumed(message.into())
    }

    fn message(&self) -> &str {
        match self {
            Self::Retryable(message) | Self::Consumed(message) => message,
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

    let goal = session.workflow.as_ref().map_or_else(
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
    use polaris_core::desktop_store::SessionManagerStore;
    use polaris_core::session_manager::SessionManagerV1;

    let root = ensure_private_directory(&state_dir.join("tui-admissions-v1"))?;
    let mut found = None;
    for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            return Err("TUI admission保存先にsymlinkがあります".into());
        }
        if !file_type.is_dir() {
            continue;
        }
        let path = fs::canonicalize(entry.path()).map_err(|error| error.to_string())?;
        let store = SessionManagerStore::open_owned(&path).map_err(|error| error.to_string())?;
        let Some(manager) = store.load().map_err(|error| error.to_string())? else {
            continue;
        };
        if manager.sessions.contains_key(current) {
            if found.is_some() {
                return Err("conversationが複数のTUI admission ownerに属しています".into());
            }
            found = Some((store, manager));
        }
    }
    if let Some(found) = found {
        return Ok(found);
    }

    let family = ensure_private_directory(&root.join(current.as_str()))?;
    let store = SessionManagerStore::open_owned(&family).map_err(|error| error.to_string())?;
    let manager = SessionManagerV1::new(current.clone());
    store
        .initialize(&manager)
        .map_err(|error| error.to_string())?;
    Ok((store, manager))
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

fn recover_tui_admission_owner(
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
        if matches!(admission.state, AdmissionState::Running { .. }) {
            manager
                .terminal(
                    &id,
                    before,
                    admission.revision,
                    TerminalStatus::OutcomeUnknown,
                )
                .map_err(|error| error.to_string())?;
        } else {
            manager
                .cancel(&id, before, admission.revision)
                .map_err(|error| error.to_string())?;
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
) -> Result<(TuiAdmissionTurn, Option<String>), TuiAdmissionFailure> {
    use polaris_core::session_manager::{
        AdmissionState, DestinationMode, NextRunnableV1, SessionId, SubmissionPayloadV1,
        SubmitOutcomeV1, SubmitRequestV1,
    };

    let persistence = session
        .persistence
        .as_ref()
        .ok_or("sidechat自動振り分けにはv2セッションが必要です")?;
    let snapshot = persistence.snapshot().map_err(|error| error.to_string())?;
    let current =
        SessionId::new(snapshot.state.session_id.clone()).map_err(|error| error.to_string())?;
    let session_revision = snapshot.state.generation;
    let goal_revision = session
        .workflow
        .as_ref()
        .map_or(0, |workflow| workflow.state().revision);
    let (store, mut manager) = open_tui_admission_owner(state_dir, &current)?;
    recover_tui_admission_owner(
        &store,
        &mut manager,
        &persistence.data_root,
        &persistence.database,
        &snapshot.state.project_id,
    )?;
    let before = manager.revision;
    manager
        .observe_session_revisions(&current, before, session_revision, goal_revision)
        .map_err(|error| error.to_string())?;
    persist_manager_transition(&store, before, &manager)?;

    let request = SubmitRequestV1 {
        parent_session: current.clone(),
        payload: SubmissionPayloadV1 {
            text: text.to_owned(),
            attachment_ids: Vec::new(),
            route: DestinationMode::Auto,
            expected_session_revision: session_revision,
            expected_goal_revision: goal_revision,
        },
        admission_id: None,
    };
    let before = manager.revision;
    let SubmitOutcomeV1::Accepted { admission, .. } = manager
        .submit(request, before)
        .map_err(|error| error.to_string())?;
    persist_manager_transition(&store, before, &manager)?;

    let before = manager.revision;
    let classifying = manager
        .begin_classification(&admission.id, before, admission.revision)
        .map_err(|error| error.to_string())?;
    persist_manager_transition(&store, before, &manager)?;
    let approval_reply = !session.clarification.unanswered().is_empty();
    let judgement = if approval_reply {
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
    let prepared = manager
        .prepare_destination(&admission.id, before, decided.revision)
        .map_err(|error| error.to_string())?;
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
    let appended_now = destination_session
        .push_admitted_user(text, &admission.id, &prepared.payload_hash)
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
        switched.then(|| {
            format!(
                "sidechat {} に切り替えました ({reason:?}, running)",
                destination.as_str()
            )
        }),
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
        let (id, origins) = saved_tool_memory_namespace(project, session_path)?;
        tool_memory::configure_tool_memory_with_origins(
            session, project, state_dir, &id, &origins, mode, embedding,
        )?;
    }
    Ok(())
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
    Interrupt,
}

fn apply_inflight_key(
    input_buffer: &mut String,
    input_cursor: &mut usize,
    input_history: &mut input::History,
    queued_inputs: &mut VecDeque<String>,
    key: ratatui::crossterm::event::KeyEvent,
) -> InflightInputAction {
    match apply_key(input_buffer, input_cursor, key) {
        InputAction::Continue => InflightInputAction::Continue,
        InputAction::Submit(text) if text.trim().is_empty() => InflightInputAction::Continue,
        InputAction::Submit(text) => {
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
    if !dispatching_queued_input {
        input_history.record(text);
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
}

fn append_live_event(
    event: &polaris_core::AgentEvent,
    history: &mut Vec<render::HistoryLine>,
    activity_rows: &mut AgentActivityRows,
) -> usize {
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

    render::render_footer(
        frame,
        footer_area,
        input,
        cursor,
        status,
        header,
        suggestions,
        selected_suggestion,
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

    // A launch without an explicit continuation starts empty: there is no
    // implicit session per project. Its JSONL id remains unwritten until the
    // first message; a prepared continuation retains its resolved v2 id.
    let mut session = match new_workflow_session(WorkflowSessionArgs {
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
        skills: args.skills,
    })
    .await
    {
        Ok(session) => session,
        Err(error) => {
            eprintln!("Can't start the workflow session: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut session_started_at_millis = now_millis();
    let session_id = session
        .persistence
        .as_ref()
        .and_then(|saved| saved.snapshot().ok())
        .map(|snapshot| snapshot.state.session_id)
        .unwrap_or_else(new_session_id);
    let mut session_path = args.sessions_dir.join(format!("{session_id}.jsonl"));
    let mut meta_path = session_path.with_extension("meta.json");

    if args
        .continuation
        .as_ref()
        .is_some_and(|continuation| continuation.kind == ContinuationKind::Resume)
        && let Err(error) = session.recover_history().await
    {
        eprintln!("strict10履歴を復元できません: {error}");
        return ExitCode::FAILURE;
    }

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
    // `ratatui::init_with_options` enables raw mode but — regardless of
    // `Viewport` — does not itself enter the alternate screen, so that's
    // done explicitly here (`EnterAlternateScreen`). `ratatui::restore()`
    // at this function's single cleanup point below already leaves the
    // alternate screen again on its own (confirmed against ratatui
    // 0.29.0's source), so nothing else needs to undo this call — a
    // fullscreen redraw never overwrites the user's real shell scrollback
    // and always hands it back on exit.
    if let Err(e) = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    ) {
        eprintln!("Can't enter the alternate screen: {e}");
        return ExitCode::FAILURE;
    }
    let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    }));
    // Trackpad/mouse wheel scrolling of the conversation history, and mouse
    // drag-to-select (see `selection.rs`) — without this, crossterm never
    // emits `Event::Mouse` at all. This blocks the terminal's own native
    // mouse handling (including drag-select) while polaris is running — see
    // `selection.rs`'s module doc for why polaris implements its own
    // selection instead of relying on that. Best-effort: a terminal that
    // doesn't support mouse reporting just keeps not sending mouse events,
    // same as before.
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
    let mut status = Status::Idle;
    let mut exit_confirmation = ExitConfirmation::default();
    let mut key_reader = CrosstermKeyReader;
    // Each turn gets its own MCP runtime; connections and grants never carry over.
    let mcp_ceiling =
        polaris_core::mcp::operation::ExternalCeiling::host_external(args.mcp_external);
    let mcp_host = polaris_core::mcp::local::LocalHost::open(&args.cwd, false).ok();
    let mcp_setup = mcp_host
        .as_ref()
        .map(|host| Arc::new(host.setup_tool(mcp_ceiling)));
    let setting = Arc::new(polaris_core::recipe::tool::SettingTool::new(
        polaris_core::project::resolve_root(&args.cwd),
    ));
    let computer = polaris_core::computer::ComputerTool::available().map(Arc::new);
    let mut mcp_turns = 0u64;
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

    let mut tool_memory_path: Option<PathBuf> = None;
    let exit_code = 'outer: loop {
        // `/new`, `/fork`, and `/resume` replace the conversation owner.
        // Reattach process-owned services and selectors before accepting the
        // next input; none of these belong to durable conversation state.
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
        let dispatching_queued_input = if let Some(queued) = queued_inputs.pop_front() {
            draft_after_queued_dispatch = Some((std::mem::take(&mut input_buffer), input_cursor));
            input_buffer = queued;
            input_cursor = input_buffer.len();
            true
        } else {
            false
        };
        if tool_memory_path.as_ref() != Some(&session_path)
            || (args.tool_memory.is_some()
                && session.tool_memory.is_none()
                && memory::saved_project_matches(&session, &args.cwd, &session_path))
        {
            if let Err(error) = configure_saved_tool_memory(
                &mut session,
                &args.cwd,
                &args.state_dir,
                &session_path,
                args.tool_memory,
                args.tool_memory_embedding.clone(),
            ) {
                fatal_message = Some(format!("ツール記憶を準備できません: {error}"));
                break 'outer ExitCode::FAILURE;
            }
            tool_memory_path = Some(session_path.clone());
        }
        let mut cumulative_usage = usage_meter.snapshot().usage;
        session.compaction_threshold = args.compact_at;
        if args.remember {
            if let Err(error) = memory::configure_saved_memory(
                &mut session,
                &args.cwd,
                &args.state_dir,
                &session_path,
            ) {
                fatal_message = Some(format!("履歴保存を準備できません: {error}"));
                break 'outer ExitCode::FAILURE;
            }
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
        let suggestions = if dispatching_queued_input {
            // Let the full command reach `slash::parse` below. The picker is
            // for interactive completion and would otherwise reinterpret a
            // queued exact command as its first highlighted prefix match.
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
                                    status = Status::Notice(apply_selection_copy(
                                        &text,
                                        clipboard::copy_to_clipboard,
                                    ));
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
        if let ratatui::crossterm::event::Event::Resize(_, _) = &event {
            selection = None;
        }
        let ratatui::crossterm::event::Event::Key(key) = event else {
            continue;
        };
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press
            && !(key.code == ratatui::crossterm::event::KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL))
        {
            exit_confirmation.cancel();
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
                    input_buffer.clear();
                    input_cursor = 0;
                    selected_suggestion = 0;
                    match action {
                        slash::Action::Review(extra) => {
                            review_text = Some(review_prompt(&extra));
                        }
                        slash::Action::Resume => {
                            handle_resume(
                                &terminal,
                                &mut key_reader,
                                &args.sessions_dir,
                                &args.state_dir,
                                &cwd_display,
                                &args.workflow_config,
                                args.workflow_mode_overridden,
                                args.skills,
                                args.history_backend_factory.as_ref(),
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                                &mut local_lines,
                            )
                            .await;
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
                            handle_new_session(
                                &args.sessions_dir,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                                &mut local_lines,
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
                            handle_fork(
                                &args.sessions_dir,
                                &cwd_display,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
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
                                &mut status,
                            );
                            continue;
                        }
                        slash::Action::Model => {
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
                            continue;
                        }
                        slash::Action::Skills => {
                            run_skills_picker(&terminal, &mut key_reader, args.skills);
                            status = Status::Idle;
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
                            handle_compact(
                                args.provider.as_ref(),
                                &mut session,
                                &session_path,
                                &mut status,
                                &mut cumulative_usage,
                            )
                            .await;
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
                                args.skills,
                                &args.cwd,
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
                                SlashOutcome::Quit => break ExitCode::SUCCESS,
                                SlashOutcome::Fatal(msg) => {
                                    fatal_message = Some(msg);
                                    break 'outer ExitCode::FAILURE;
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
            status = Status::Notice(clipboard::copy_last_reply_with(
                &session,
                clipboard::copy_to_clipboard,
            ));
            continue;
        }

        selection = None;

        let text = if let Some(t) = review_text {
            t
        } else {
            let typed = match apply_key(&mut input_buffer, &mut input_cursor, key) {
                InputAction::Continue => {
                    exit_confirmation.cancel();
                    selected_suggestion = 0;
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
                InputAction::Submit(text) => {
                    exit_confirmation.cancel();
                    // In-flight submissions were recorded when Enter queued
                    // them. Recording the synthetic Enter again would add a
                    // second copy after any later queued entry.
                    record_submission(&mut input_history, &text, dispatching_queued_input);
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
            match slash::parse(&typed) {
                Some(slash::Action::Review(extra)) => review_prompt(&extra),
                Some(slash::Action::Resume) => {
                    handle_resume(
                        &terminal,
                        &mut key_reader,
                        &args.sessions_dir,
                        &args.state_dir,
                        &cwd_display,
                        &args.workflow_config,
                        args.workflow_mode_overridden,
                        args.skills,
                        args.history_backend_factory.as_ref(),
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                        &mut local_lines,
                    )
                    .await;
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
                    handle_new_session(
                        &args.sessions_dir,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                        &mut local_lines,
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
                    handle_fork(
                        &args.sessions_dir,
                        &cwd_display,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
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
                        &mut status,
                    );
                    continue;
                }
                Some(slash::Action::Model) => {
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
                    continue;
                }
                Some(slash::Action::Skills) => {
                    run_skills_picker(&terminal, &mut key_reader, args.skills);
                    status = Status::Idle;
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
                    handle_compact(
                        args.provider.as_ref(),
                        &mut session,
                        &session_path,
                        &mut status,
                        &mut cumulative_usage,
                    )
                    .await;
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
                        args.skills,
                        &args.cwd,
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
                        SlashOutcome::Quit => break ExitCode::SUCCESS,
                        SlashOutcome::Fatal(msg) => {
                            fatal_message = Some(msg);
                            break 'outer ExitCode::FAILURE;
                        }
                    }
                }
                None => typed,
            }
        };

        session.automatic_effort =
            args.initial_effort_name.is_none() && !is_local_provider(&provider_name);
        if session.automatic_effort {
            effort_name = UNDECIDED_EFFORT.to_string();
        }
        let mut tui_admission = None;
        if args.sidechat_auto && session.persistence.is_some() {
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
            )
            .await
            {
                Ok((admission, notice)) => {
                    tui_admission = Some(admission);
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
                Err(error @ TuiAdmissionFailure::Consumed(_)) => {
                    fatal_message = Some(format!(
                        "入力は保存済みですが、実行開始を確認できません: {}。再起動後に回復状態を確認してください。",
                        error.message()
                    ));
                    break 'outer ExitCode::FAILURE;
                }
            }
        } else {
            if let Some(installer) = &args.turn_authority
                && let Err(error) = installer(&mut session, &text)
            {
                input_buffer = text;
                input_cursor = input_buffer.len();
                status = Status::Error(format!("routine authorityを接続できません: {error}"));
                continue;
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
                fatal_message = Some(format!("Can't write the session metadata: {e}"));
                break 'outer ExitCode::FAILURE;
            }
            if let Err(e) = persist::append_message(
                &session_path,
                session.messages.last().expect("just pushed"),
            ) {
                fatal_message = Some(format!("Can't persist the message: {e}"));
                break 'outer ExitCode::FAILURE;
            }
        }
        if let Err(e) = session.check_persistence() {
            fatal_message = Some(format!("Can't persist the message: {e}"));
            break 'outer ExitCode::FAILURE;
        }

        let turn_started = Instant::now();
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
            break ExitCode::FAILURE;
        }

        enum TurnOutcome {
            Done(Result<agent::AgentOutcome, agent::AgentError>),
            Interrupted,
            Fatal,
        }

        // Scoped so every borrow taken to build `agent_future` (of
        // `session`, `audit`, ...) ends when this block does — the match
        // below needs `&mut session` again, which it can't have while
        // any of these are still alive.
        session.mcp_setup = mcp_setup.clone();
        mcp_turns += 1;
        let mut mcp_runtime = mcp_host.as_ref().and_then(|host| {
            host.runtime("tui", &format!("turn-{mcp_turns}"), mcp_ceiling)
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
        });
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
                status = Status::Error(format!("subagent controlを準備できません: {error}"));
                continue 'outer;
            }
        };
        let outcome = {
            let mut stop = StopTracker::new(args.max_turns);
            let mut gate = Gate::new(approval_policy);
            let mut approver = TuiApprover {
                terminal: &terminal,
                reader: &mut key_reader,
            };
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
                args.skills,
                args.agent_types,
                args.provider.clone(),
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
            let mut event_stream = ratatui::crossterm::event::EventStream::new();
            // How long the `Ctrl+O` copy confirmation stays on screen
            // before the ticker branch resumes overwriting `status` with
            // the `Thinking` animation — without this, the confirmation
            // would only ever survive until the very next tick (at most
            // 100ms later), which reads as no feedback at all.
            let mut mid_turn_notice_until: Option<Instant> = None;

            let turn_outcome = loop {
                tokio::select! {
                    biased;
                    result = &mut agent_future => break TurnOutcome::Done(result),
                    // Ahead of the tick and the key-event arms so a burst
                    // of tool activity is printed as it happens rather
                    // than queueing behind cosmetic redraws; behind
                    // `agent_future` so the turn's own completion (and
                    // the rollback it may need) is never delayed by
                    // display work. Anything still queued when the turn
                    // finishes is drained right after this loop.
                    Some(event) = events_rx.recv() => {
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
                                    KeyCode::Esc => break TurnOutcome::Interrupted,
                                    KeyCode::PageUp => {
                                        scroll_offset = scroll_offset.saturating_add(1);
                                    }
                                    KeyCode::PageDown => {
                                        scroll_offset = scroll_offset.saturating_sub(1);
                                    }
                                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        status = Status::Notice(clipboard::copy_text_with(
                                            reply_before_this_turn.as_deref(),
                                            clipboard::copy_to_clipboard,
                                        ));
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
                                    _ => match apply_inflight_key(
                                        &mut input_buffer,
                                        &mut input_cursor,
                                        &mut input_history,
                                        &mut queued_inputs,
                                        key,
                                    ) {
                                        InflightInputAction::Continue => {}
                                        InflightInputAction::Queued => {
                                            status = Status::Notice(format!(
                                                "queued input ({})",
                                                queued_inputs.len()
                                            ));
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
                                            status = Status::Notice(apply_selection_copy(
                                                &text,
                                                clipboard::copy_to_clipboard,
                                            ));
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
        if let Some(runtime) = mcp_runtime.as_mut() {
            // An unconfirmed stop parks the child with the shared cleanup owner.
            let _ = runtime.shutdown().await;
        }
        // Only a normally completed root turn can prove that every child
        // reached a terminal state.  Provider/tool errors can abandon a
        // spawn wave just as interruption can, so fail closed and mark any
        // still-running child outcome unknown before releasing the owner.
        let cancel_children = !matches!(&outcome, TurnOutcome::Done(Ok(_)));
        let mut control_cleanup_error = session
            .finish_root_delegation_control(cancel_children)
            .await
            .err();
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
            };
            if let Err(error) = admission.finish(terminal) {
                control_cleanup_error = Some(format!("sidechat runを終了できません: {error}"));
            }
        }
        if matches!(&outcome, TurnOutcome::Done(Ok(_)))
            && let Some(finalizer) = args.answer_finalizer.as_ref()
        {
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
            TurnOutcome::Fatal => break ExitCode::FAILURE,
        }
        if let Some(error) = control_cleanup_error {
            status = Status::Error(format!("subagent controlを終了できません: {error}"));
        }
    };

    // Already leaves the alternate screen entered above, on its own
    // (confirmed against ratatui 0.29.0's source) — no separate explicit
    // `LeaveAlternateScreen` call is needed here.
    ratatui::restore();
    // Restore the terminal's own default cursor shape, undoing the
    // BlinkingBar set at startup — best-effort, same as the set itself.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    // Undoes the EnableMouseCapture set at startup — best-effort, same as
    // the enable itself. Otherwise mouse reporting mode would leak into
    // whatever the user's shell does next (e.g. text selection with the
    // mouse would stop working until they open and close another
    // mouse-reporting program).
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
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
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

    if selected.kind == sessions::SessionKind::V2 {
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
        let result = async {
            let persistence = polaris_core::session_store::PersistedSession::open(
                data_root,
                &state_dir.join("memory.sqlite3"),
                project_id,
                &selected.id,
            )
            .map_err(|error| error.to_string())?;
            let mut restored = polaris_core::session::Session::new();
            restored.disable_files_md_auto_regenerate = disable_files_md_auto_regenerate;
            restored.input_preparation = session.input_preparation;
            restored.strict_history = strict_history;
            restored.summary_usage = summary_usage;
            restored.examples = examples;
            restore_prepared_continuation(
                &mut restored,
                &PreparedContinuation {
                    kind: ContinuationKind::Resume,
                    persistence,
                },
                workflow_config,
                workflow_mode_overridden,
                skills,
                history_backend_factory,
            )
            .await?;
            Ok::<_, String>(restored)
        }
        .await;
        match result {
            Ok(mut restored) => {
                if let Err(error) = restored.recover_history().await {
                    *status = Status::Notice(format!("strict10履歴を復元できません: {error}"));
                    return;
                }
                *session = restored;
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

    let new_session_path = selected.path;
    let new_meta_path = new_session_path.with_extension("meta.json");
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
                match polaris_core::session_store::PersistedSession::import(
                    data_root,
                    &state_dir.join("memory.sqlite3"),
                    project_id,
                    mode,
                    workflow.as_ref(),
                    &loaded.messages,
                ) {
                    Ok(persistence) => {
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
fn handle_new_session(
    sessions_dir: &std::path::Path,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    if let Some(previous) = session.persistence.clone() {
        let strict_history = session.strict_history.clone();
        let summary_usage = session.summary_usage.clone();
        let examples = session.examples.clone();
        let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
        let input_preparation = session.input_preparation;
        let snapshot = match previous.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                *status = Status::Notice(format!("新しいv2セッションを作成できません: {error}"));
                return;
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
        let persistence = polaris_core::session_store::PersistedSession::create(
            &previous.data_root,
            &previous.database,
            &snapshot.state.project_id,
            snapshot.state.history_mode,
            next.workflow.as_ref(),
        );
        match persistence.and_then(|persistence| {
            next.attach(persistence)?;
            Ok(())
        }) {
            Ok(()) => {
                next.strict_history = strict_history;
                next.summary_usage = summary_usage;
                next.examples = examples;
                *session = next;
            }
            Err(error) => {
                *status = Status::Notice(format!("新しいv2セッションを作成できません: {error}"));
                return;
            }
        }
    } else {
        let disable_files_md_auto_regenerate = session.disable_files_md_auto_regenerate;
        let input_preparation = session.input_preparation;
        *session = polaris_core::session::Session::default();
        session.disable_files_md_auto_regenerate = disable_files_md_auto_regenerate;
        session.input_preparation = input_preparation;
    }
    *session_path = sessions_dir.join(format!("{}.jsonl", new_session_id()));
    *meta_path = session_path.with_extension("meta.json");
    *session_started_at_millis = now_millis();
    local_lines.clear();
    *status = Status::Idle;
}

/// Handles `/fork`: like `/new`, rotates to a brand-new file/id without
/// touching the one being left, except the new one is seeded with every
/// message so far instead of starting empty — codex's own `/fork`
/// ("fork the current chat") does the same. The seeded messages are
/// written to disk immediately (unlike a fresh `/new` session, which
/// only starts existing on disk lazily at the first new message) — a
/// fork with no messages yet added since forking would otherwise vanish
/// from `/resume`'s list, which defeats the point of forking one.
fn handle_fork(
    sessions_dir: &std::path::Path,
    cwd_display: &str,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
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
        let persistence = previous.fork("");
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

/// Runs the `/permissions` picker as its own small blocking loop, the
/// same shape as `run_resume_picker` (three fixed rows instead of a
/// dynamic list, so no empty-state branch is needed). Returns the picked
/// policy, or `None` if the user backed out with Esc.
fn run_permissions_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    current: ApprovalPolicy,
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
            .draw(|f| render::render_permissions_picker(f, current, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % OPTIONS.len(),
            Ok(KeyCode::Up) => selected = (selected + OPTIONS.len() - 1) % OPTIONS.len(),
            Ok(KeyCode::Enter) => return Some(OPTIONS[selected]),
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
    status: &mut Status,
) {
    if let Some(picked) = run_permissions_picker(terminal, reader, *approval_policy) {
        *approval_policy = picked;
        *status = Status::Notice(format!("permissions set to {picked:?}"));
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
        let current_is_local = is_local_provider(provider_name);
        let Some(choice) = run_model_picker(terminal, reader, model_name, current_is_local) else {
            return;
        };
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
                let Some(index) =
                    run_local_model_picker(terminal, reader, &models, provider_name, model_name)
                else {
                    if models.is_empty() {
                        *status = Status::Notice("no connected local models".to_string());
                        return;
                    }
                    continue;
                };
                let picked = &models[index];
                let selection = match picked.adapter.select(&picked.observed).await {
                    Ok(selection) => selection,
                    Err(error) => {
                        *status = Status::Error(format!("local model is unavailable: {error}"));
                        return;
                    }
                };
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
) {
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
}

/// Runs the `/skills` picker: a blocking loop like the others, but with
/// no selection outcome — Enter and Esc both just close it (see
/// `render::render_skills_picker`'s docs on why: polaris has nothing
/// analogous to codex's enable/disable state to act on). Up/Down still
/// move a highlight for visual consistency with every other picker, even
/// though nothing currently reads which row is highlighted; that's ready
/// to become a scroll anchor if a project's skill list ever grows past
/// what a single screen holds, but that's not implemented yet.
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
    let mut out = String::from("# polaris conversation\n\n");
    for m in &session.messages {
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
fn mcp_state_lines(cwd: &std::path::Path) -> Vec<String> {
    let Ok(host) = polaris_core::mcp::local::LocalHost::open(cwd, false) else {
        return vec!["MCPの設定を読めません。".into()];
    };
    // The report is what a run would admit, so it is read under the same
    // ceiling a run without `--mcp-external` would use: a stricter view
    // here would show servers a run can actually use as unavailable.
    let report = match host
        .state_report(polaris_core::mcp::operation::ExternalCeiling::host_external(false))
    {
        Ok(report) => report,
        Err(error) => return vec![format!("MCPの状態を読めません: {error}")],
    };
    if report.is_empty() {
        return vec!["登録されたMCPサーバーはありません。".into()];
    }
    let mut lines = vec![format!("MCPサーバー {}件", report.len())];
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

#[allow(clippy::too_many_arguments)]
fn apply_slash_action(
    action: slash::Action,
    session: &mut polaris_core::session::Session,
    session_path: &std::path::Path,
    status: &mut Status,
    provider_name: &str,
    model_name: &str,
    usage_report: polaris_provider::UsageReport,
    // No longer read here — `/skills` is intercepted in `run()` before
    // dispatch (see `run_skills_picker`) now that it's a full-screen
    // picker instead of a one-line `Status::Notice`. Kept as a parameter
    // rather than removed, since removing it would mean touching every
    // `apply_slash_action`/`call` call site (production and test) for a
    // signature change with no behavior difference.
    skills: &[polaris_skills::Skill],
    cwd: &std::path::Path,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) -> SlashOutcome {
    match action {
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
            let cumulative_usage = usage_report.usage;
            let workflow = session
                .workflow
                .as_ref()
                .map(|workflow| {
                    let state = workflow.state();
                    let ids = state
                        .skills
                        .iter()
                        .map(|skill| skill.id.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "; workflow phase: {}; skills: {}; manifest: {}",
                        state.phase, ids, state.skill_manifest_hash
                    )
                })
                .unwrap_or_default();
            *status = Status::Notice(format!(
                "{provider_name} / {model_name} — tokens: in {} / out {} / cache {} / total {} — {} messages{workflow}; 確認済み使用量、欠測 {} / 失敗 {}",
                cumulative_usage.input_tokens,
                cumulative_usage.output_tokens,
                cumulative_usage.cached_tokens,
                cumulative_usage.total_tokens,
                session.messages.len(),
                usage_report.missing_responses,
                usage_report.failed_requests,
            ));
            SlashOutcome::Continue
        }
        slash::Action::Phase(requested) => {
            let Some(workflow) = &session.workflow else {
                *status = Status::Notice("workflowはこのセッションで無効です".into());
                return SlashOutcome::Continue;
            };
            if requested.is_empty() {
                *status = Status::Notice(format!("ワークフロー段階: {}", workflow.state().phase));
                return SlashOutcome::Continue;
            }
            let configured = (|| -> Result<polaris_core::workflow::SessionWorkflow, String> {
                session
                    .check_persistence()
                    .map_err(|error| error.to_string())?;
                let phase = requested.parse::<polaris_core::workflow::Phase>()?;
                let candidate = polaris_core::workflow::SessionWorkflow::restore(
                    workflow.config.clone(),
                    workflow.gates.clone(),
                    workflow.state(),
                )?;
                candidate.request_phase(phase)?;
                if candidate.config.enabled {
                    drop(candidate.begin_turn_from_disk(skills)?);
                }
                if let Some(saved) = &session.persistence {
                    saved
                        .checkpoint(Some(&candidate))
                        .map_err(|error| error.to_string())?;
                }
                Ok(candidate)
            })();
            match configured {
                Ok(candidate) => {
                    *status =
                        Status::Notice(format!("ワークフロー段階: {}", candidate.state().phase));
                    session.workflow = Some(candidate);
                }
                Err(error) => *status = Status::Notice(format!("phaseを保存できません: {error}")),
            }
            SlashOutcome::Continue
        }
        slash::Action::Workflow(requested) => {
            if let Err(error) = session.check_persistence() {
                *status = Status::Notice(format!("workflow設定を保存できません: {error}"));
                return SlashOutcome::Continue;
            }
            let Some(current) = &session.workflow else {
                *status = Status::Notice("workflowはこのセッションで無効です".into());
                return SlashOutcome::Continue;
            };
            if requested.is_empty() {
                *status = Status::Notice(format!(
                    "workflow: {}",
                    if current.config.enabled {
                        match current.config.mode {
                            polaris_core::workflow::WorkflowMode::Manual => "manual",
                            polaris_core::workflow::WorkflowMode::Adaptive => "adaptive",
                        }
                    } else {
                        "off"
                    }
                ));
                return SlashOutcome::Continue;
            }
            let mode = requested.strip_prefix("mode ").unwrap_or(&requested);
            let mut config = current.config.clone();
            match mode {
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
                _ => {
                    *status = Status::Notice(
                        "workflow modeは manual、adaptive、off のいずれかです".into(),
                    );
                    return SlashOutcome::Continue;
                }
            }
            let candidate = match polaris_core::workflow::SessionWorkflow::restore(
                config,
                current.gates.clone(),
                current.state(),
            ) {
                Ok(candidate) => candidate,
                Err(error) => {
                    *status = Status::Notice(format!("workflow設定を検証できません: {error}"));
                    return SlashOutcome::Continue;
                }
            };
            if candidate.config.enabled
                && let Err(error) = candidate.begin_turn_from_disk(skills)
            {
                *status = Status::Notice(format!("workflow設定を検証できません: {error}"));
                return SlashOutcome::Continue;
            }
            let Some(persistence) = &session.persistence else {
                *status = Status::Notice("workflow設定を保存するセッションがありません".into());
                return SlashOutcome::Continue;
            };
            if let Err(error) = persistence.checkpoint(Some(&candidate)) {
                *status = Status::Notice(format!("workflow設定を保存できません: {error}"));
                return SlashOutcome::Continue;
            }
            session.workflow = Some(candidate);
            *status = Status::Notice(format!("workflow: {mode}"));
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
            *status = Status::Notice(cwd.display().to_string());
            SlashOutcome::Continue
        }
        slash::Action::Mcp => {
            for line in mcp_state_lines(cwd) {
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
            // Read-only, so it runs directly rather than through the
            // sandbox (which exists to confine what the *model* can
            // mutate, not to gate the user's own read commands).
            const MAX_DIFF_LINES: usize = 200;
            let text = match std::process::Command::new("git")
                .arg("diff")
                .current_dir(cwd)
                .output()
            {
                Ok(out) if out.status.success() => {
                    if out.stdout.is_empty() {
                        "(no changes)".to_string()
                    } else {
                        String::from_utf8_lossy(&out.stdout).into_owned()
                    }
                }
                Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
                Err(e) => format!("can't run git diff: {e}"),
            };
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
            *status = Status::Idle;
            SlashOutcome::Continue
        }
        slash::Action::Review(_)
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
            let outcome = polaris_auth::store::default_path()
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    polaris_auth::logout(&p)
                        .map(|removed| (p, removed))
                        .map_err(|e| e.to_string())
                });
            *status = match outcome {
                Ok((p, true)) => Status::Notice(format!("logged out: {}", p.display())),
                Ok((_, false)) => Status::Notice("not logged in".to_string()),
                Err(e) => Status::Notice(format!("can't log out: {e}")),
            };
            SlashOutcome::Continue
        }
        slash::Action::Help => {
            let list = slash::COMMANDS
                .iter()
                .map(|c| format!("/{}", c.name))
                .collect::<Vec<_>>()
                .join("  ");
            *status = Status::Notice(format!("commands: {list}"));
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
    struct DistinctContextJudge;

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

        let (turn, notice) = admit_tui_input(
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
        )
        .await
        .unwrap();
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
    }

    #[cfg(unix)]
    #[test]
    fn recovery_proves_a_committed_input_before_cancelling_an_unstarted_run() {
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
        persist_manager_transition(&store, before, &manager).unwrap();
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

        recover_tui_admission_owner(&store, &mut manager, root.path(), &database, "project")
            .unwrap();
        assert!(matches!(
            manager.status(&admission.id).unwrap().state,
            AdmissionState::Terminal {
                status: TerminalStatus::Cancelled,
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
        let owner = polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner::open_configured(
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
    fn status_includes_the_cached_token_count() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        let usage = polaris_provider::Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_tokens: 80,
        };

        apply_slash_action(
            slash::Action::Status,
            &mut session,
            &session_path,
            &mut status,
            "openai",
            "gpt-5.4",
            polaris_provider::UsageReport {
                usage,
                ..Default::default()
            },
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => {
                assert!(n.contains("cache 80"), "expected a cache figure in: {n}");
                assert!(n.contains("in 100"));
                assert!(n.contains("out 20"));
                assert!(n.contains("total 120"));
            }
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn phase_command_changes_an_enabled_workflow_and_rejects_unknown_names() {
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
            polaris_core::workflow::Phase::Review
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
            polaris_core::workflow::Phase::Review,
            "an idle /phase change must be durable before the next restart"
        );
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
            polaris_core::workflow::Phase::Review
        );
    }

    #[test]
    fn workflow_command_reconfigures_and_checkpoints_before_exposing_mode() {
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
            polaris_core::workflow::WorkflowMode::Adaptive
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
            polaris_core::workflow::WorkflowMode::Adaptive
        );

        call(
            slash::Action::Workflow("off".into()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );
        assert!(!session.workflow.as_ref().unwrap().config.enabled);
    }

    #[test]
    fn pwd_reports_the_working_directory() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Pwd,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => assert_eq!(n, dir.path().display().to_string()),
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
            slash::Action::Mcp,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(local_lines.len(), 1, "{local_lines:?}");
        assert!(
            local_lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains("登録されたMCPサーバーはありません")),
            "{local_lines:?}"
        );
        // A local command: nothing is sent and nothing joins the history.
        assert!(session.messages.is_empty());
        assert!(matches!(status, Status::Idle));
    }

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
    fn handle_permissions_switches_to_the_picked_policy() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // Down from Never (index 0) to OnRequest (index 1), then Enter.
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        let mut approval_policy = ApprovalPolicy::Never;
        let mut status = Status::Idle;

        handle_permissions(&terminal, &mut reader, &mut approval_policy, &mut status);

        assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
        match status {
            Status::Notice(n) => assert!(n.contains("OnRequest")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn handle_permissions_cancelled_with_esc_leaves_the_policy_untouched() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
        let mut approval_policy = ApprovalPolicy::Never;
        let mut status = Status::Idle;

        handle_permissions(&terminal, &mut reader, &mut approval_policy, &mut status);

        assert_eq!(approval_policy, ApprovalPolicy::Never);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn diff_reports_no_changes_outside_a_git_repository() {
        // `cwd` here is a bare tempdir, not a git repo — `git diff` exits
        // non-zero and prints to stderr, which must show up as content
        // rather than a crash (a missing/broken git repo is a common,
        // recoverable situation, not a fatal one).
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
