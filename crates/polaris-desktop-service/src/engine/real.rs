//! Explicit trusted production integration; no provider discovery or source apply.
use super::*;
use polaris_core::{
    AgentEvent,
    agent::AgentError,
    audit::AuditLog,
    desktop_events::{
        DesktopEventReceiver, DesktopEventSink, EVENT_CAPACITY, EventEnvelope, EventFailure,
    },
    desktop_run::{DesktopRequestContext, DesktopRun, DesktopRunError, DesktopRunInput},
    desktop_store::{HistoryGap, SavedWorkflow},
    isolated_run::PreparedWorkspace,
    session::Session,
};
use polaris_provider::{Message, Provider, Role};

fn delegation_projection_from(
    binding: &polaris_core::desktop_store::SubagentControlBindingV1,
    snapshot: &polaris_core::desktop_store::SubagentControlSnapshotV1,
) -> polaris_core::session::RootDelegationProjectionV1 {
    let children = snapshot
        .control_plane
        .children
        .iter()
        .filter_map(|child| {
            child
                .attempts
                .last()
                .map(|attempt| polaris_core::session::ChildControlProjectionV1 {
                    logical_child_id: child.logical_child_id.clone(),
                    attempt_id: attempt.attempt_id.clone(),
                    display_name: child.display_name.clone(),
                    role: child.role.clone(),
                    tier: child.tier.clone(),
                    model: child.model.clone(),
                    effort: child.effort.clone(),
                    state: attempt.state.clone(),
                    activity: attempt.activity.clone(),
                })
        })
        .collect();
    polaris_core::session::RootDelegationProjectionV1 {
        revision: snapshot.revision,
        session_id: binding.session_id.as_str().to_owned(),
        root_run_id: binding.root_run_id.clone(),
        root_attempt_id: binding.root_attempt_id.clone(),
        active_task_id: binding.active_task_id.clone(),
        children,
        // MCP snapshot leases are in-memory capabilities and are deliberately
        // never restored after a process boundary.
        mcp_leases: Vec::new(),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "runtime setup keeps independently persisted desktop inputs explicit"
)]
fn create_delegation_runtime(
    root: &DesktopRoot,
    project_id: &ProjectId,
    published: &Published,
    target: &RunTarget,
    configuration: &Configuration,
    workflow: Option<&polaris_core::workflow::SessionWorkflow>,
    prepared: &PreparedWorkspace,
    has_mcp: bool,
) -> Result<DelegationRuntime, ServiceError> {
    use polaris_core::{
        session::{RootDelegationConfigV1, RootDelegationControlV1},
        subagent_control::{
            ApprovalCapabilityV1, CapabilitySnapshotV1, McpCapabilityV1, RootAttemptId, RootRunId,
            SandboxCapabilityV1, TaskId,
        },
    };
    let active_task = workflow
        .and_then(|workflow| workflow.state().active_task)
        .map(|task| task.task_id)
        .unwrap_or_else(|| format!("task-{}", target.run_id.as_str()));
    let root_run_id = RootRunId::new(target.run_id.as_str()).map_err(|_| ServiceError::Worker)?;
    let root_attempt_id =
        RootAttemptId::new(target.attempt_id.as_str()).map_err(|_| ServiceError::Worker)?;
    let active_task_id = TaskId::new(active_task).map_err(|_| ServiceError::Worker)?;
    let binding = polaris_core::desktop_store::SubagentControlBindingV1 {
        project_id: project_id.clone(),
        session_id: published.marker.session_id.clone(),
        root_run_id: root_run_id.clone(),
        root_attempt_id: root_attempt_id.clone(),
        active_task_id: active_task_id.clone(),
    };
    let owner = root.subagent_control(binding.clone())?;
    let owner_snapshot = owner.snapshot()?;
    let initial_projection = delegation_projection_from(&binding, &owner_snapshot);
    let owner = Arc::new(std::sync::Mutex::new(owner));
    let owner_for_hook = owner.clone();
    let binding_for_hook = binding.clone();
    let persistence_hook = Arc::new(
        move |checkpoint: polaris_core::session::RootDelegationCheckpointV1| {
            if checkpoint.binding.session_id != binding_for_hook.session_id.as_str()
                || checkpoint.binding.root_run_id != binding_for_hook.root_run_id
                || checkpoint.binding.root_attempt_id != binding_for_hook.root_attempt_id
                || checkpoint.binding.active_task_id != binding_for_hook.active_task_id
                || checkpoint.revision != checkpoint.expected_revision.saturating_add(1)
            {
                return Err("delegation checkpoint binding mismatch".to_owned());
            }
            let mut owner = owner_for_hook
                .lock()
                .map_err(|_| "delegation store owner unavailable".to_owned())?;
            let saved = owner
                .replace(checkpoint.expected_revision, checkpoint.control_plane)
                .map_err(|error| error.to_string())?;
            if saved.revision != checkpoint.revision || saved.binding != binding_for_hook {
                return Err("delegation checkpoint publication mismatch".to_owned());
            }
            Ok(())
        },
    );
    let policy = prepared.policy();
    let sandbox = match policy.mode() {
        polaris_sandbox::SandboxMode::ReadOnly => SandboxCapabilityV1::ReadOnly,
        polaris_sandbox::SandboxMode::WorkspaceWrite => SandboxCapabilityV1::WorkspaceWrite,
        polaris_sandbox::SandboxMode::FullAccess => SandboxCapabilityV1::FullAccess,
    };
    let mut tool_names = polaris_tools::all_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    tool_names.sort();
    tool_names.dedup();
    let config = RootDelegationConfigV1 {
        session_id: published.marker.session_id.as_str().to_owned(),
        root_run_id,
        root_attempt_id,
        active_task_id,
        parent_capabilities: CapabilitySnapshotV1 {
            sandbox,
            write_roots: policy
                .writable_roots()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            tool_names,
            approval: ApprovalCapabilityV1::TrustedOwnerOnly,
            mcp: if has_mcp {
                McpCapabilityV1::ReadSnapshotsOnly
            } else {
                McpCapabilityV1::None
            },
        },
        model: configuration.model.clone(),
        effort: configuration.effort.clone(),
    };
    let control = RootDelegationControlV1::restore_with_persistence_hook(
        config,
        owner_snapshot.control_plane,
        owner_snapshot.revision,
        persistence_hook,
    )
    .map_err(|_| ServiceError::Worker)?;
    let projection = Arc::new(std::sync::RwLock::new(initial_projection));
    let projection_for_hook = projection.clone();
    let (updates, receiver) = std::sync::mpsc::sync_channel(1);
    control.set_projection_hook(Some(Arc::new(move |next| {
        if let Ok(mut current) = projection_for_hook.write()
            && *current != next
        {
            *current = next;
            let _ = updates.try_send(());
        }
    })));
    Ok(DelegationRuntime {
        control,
        projection,
        updates: receiver,
    })
}

fn revoke_root_delegation(
    control: Arc<polaris_core::session::RootDelegationControlV1>,
) -> Result<(), ServiceError> {
    std::thread::Builder::new()
        .name("polaris-delegation-revoke".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ServiceError::Worker)?;
            runtime.block_on(control.revoke_root_terminal());
            Ok::<(), ServiceError>(())
        })
        .map_err(|_| ServiceError::Worker)?
        .join()
        .map_err(|_| ServiceError::Worker)??;
    Ok(())
}

/// Trusted owner only. Called after the accepted input and run intent are durable.
/// Runs on the single owned prepare thread, never the reactor or Writer owner.
/// Must not start providers/tools. Own cleanup of partial preparation on error
/// or panic; returning an error does not establish that preparation had no effects.
/// The snapshot is the durable authority, including the accepted user message.
/// Skills and agent types are immutable trusted snapshots; no host disk reload.
/// Legacy history hooks remain unavailable.
pub trait TrustedRunFactory: Send + Sync {
    /// A startup-owner validated workflow profile. Wire input never supplies
    /// skill IDs, paths, or bodies.
    fn workflow_config(&self) -> Option<&polaris_core::workflow::WorkflowConfig> {
        None
    }

    fn input_preparation(&self) -> polaris_core::input_preparation::InputPreparationMode {
        polaris_core::input_preparation::InputPreparationMode::Off
    }

    /// The typed judge this owner was started with, if any. Default `None`
    /// so a factory that has no opinion keeps the behaviour every run had
    /// before the judge existed.
    fn decider(&self) -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
        None
    }

    /// Metadata only; no dispatch or connection probe. Scoped to this owner,
    /// not the selected session. Absent when the factory has no diagnostics.
    fn decision_status(&self) -> Option<Vec<String>> {
        None
    }

    /// Startup-owned shadow finalizer. Wire and project data cannot provide it.
    fn answer_finalizer(
        &self,
    ) -> Option<std::sync::Arc<crate::shadow_finalizer::ShadowFinalizerOwner>> {
        None
    }

    fn workflow_skills(&self) -> &[polaris_skills::Skill] {
        &[]
    }
    /// Immutable trusted catalog used to validate next-run role choices.
    fn role_catalog(&self) -> &[polaris_skills::AgentType] {
        &[]
    }

    /// The registration an accepted run is fixed to, decided once, just before
    /// the acceptance that saves it. `None` keeps the single authentication.
    fn bind_account(
        &self,
    ) -> Result<Option<polaris_core::desktop_store::SavedAccount>, ServiceError> {
        Ok(None)
    }

    fn prepare(
        &self,
        target: &RunTarget,
        published: &Published,
    ) -> Result<TrustedRunInputs, ServiceError>;

    /// Bounded ownership handoff after core/native cleanup and durable finish.
    /// Queue the receipt for the trusted controller; do not perform source I/O
    /// here. This is called at most once, independently of UI event delivery.
    /// It is neither source-write permission nor a request to retry a run.
    fn completed(&self, _completion: TrustedRunCompletion) {}
}

/// Engine-created evidence that the copy has no remaining engine-owned writers.
/// Fields are private so a caller cannot manufacture this lifecycle receipt.
/// Source approval, current policy, conflicts and recovery still need validation.
pub struct TrustedRunCompletion {
    project_id: ProjectId,
    session_id: SessionId,
    target: RunTarget,
    prepared: Arc<PreparedWorkspace>,
    terminal: polaris_core::desktop_store::RunRecord,
    session_revision: DecimalU64,
}
impl TrustedRunCompletion {
    #[cfg(test)]
    pub(crate) fn for_source_apply_test(
        prepared: Arc<PreparedWorkspace>,
        saved: &Published,
    ) -> Self {
        let terminal = saved.state.runs.last().unwrap().clone();
        Self {
            project_id: saved.marker.project_id.clone(),
            session_id: saved.marker.session_id.clone(),
            target: RunTarget {
                run_id: terminal.run.run_id.clone(),
                attempt_id: terminal.run.attempt_id.clone(),
            },
            prepared,
            terminal,
            session_revision: saved.marker.session_revision,
        }
    }
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    pub fn target(&self) -> &RunTarget {
        &self.target
    }
    pub fn prepared(&self) -> &PreparedWorkspace {
        &self.prepared
    }
    pub fn terminal(&self) -> &polaris_core::desktop_store::RunRecord {
        &self.terminal
    }
    pub fn session_revision(&self) -> DecimalU64 {
        self.session_revision
    }
}

/// Already resolved inputs. Both unmetered providers are wrapped by the engine
/// with the same externally retained meter, including on cancellation/error.
/// The factory may supply immutable RoleProvider wrappers sharing a LocalRouter;
/// this engine neither discovers role bindings nor replaces their routing.
/// Source changes remain in `prepared`: this integration never applies them.
/// A trusted owner needing later inspection must retain its own Arc. Successful
/// run completion means core execution and transcript persistence, not source apply.
pub struct TrustedRunInputs {
    pub prepared: Arc<PreparedWorkspace>,
    /// Explicit confirmed copy-only mutation scope; never source-write authority.
    /// The execution owner validates it against the prepared workspace.
    pub mutation_policy: Option<polaris_core::desktop_execution::SandboxPolicy>,
    /// Confirmed copy execution ceiling, independent of mutation/source grants.
    pub execution_capability: Option<polaris_core::desktop_execution::ConfirmedExecutionCapability>,
    pub provider: Arc<dyn Provider>,
    pub provider_pool: Arc<dyn Provider>,
    pub always_on: polaris_core::prompt::AlwaysOn,
    pub audit: Arc<tokio::sync::Mutex<AuditLog>>,
    pub max_turns: u32,
    pub skills: Vec<polaris_skills::Skill>,
    pub agent_types: Vec<polaris_skills::AgentType>,
    /// DesktopRun validates both limits before starting the worker.
    pub spawn_concurrency: usize,
    pub spawn_write_concurrency: usize,
    /// Preopened trusted backend only; never resolved from a model-supplied path.
    pub tool_memory: Option<polaris_core::tool_memory::ToolMemory>,
    pub history: Option<TrustedHistoryResources>,
    pub mcp: Option<TrustedMcpResources>,
    /// Fixed from the confirmed tier; bounds MCP calls and registration.
    pub external_ceiling: polaris_core::mcp::operation::ExternalCeiling,
}

/// Fixed host configuration and external ceiling, never model-supplied. The
/// trusted resource owner retains any dedicated stdio directories through cleanup.
#[derive(Clone)]
pub struct TrustedMcpResources {
    pub config: polaris_core::mcp::runtime::RuntimeConfig,
    /// Fixed light model for tool preselection; `None` for local backends.
    pub selection_provider: Option<Arc<dyn Provider>>,
}

/// Verified before provider admission; a dedicated summary client and per-run meter.
pub struct TrustedHistoryResources {
    pub summary_provider: Arc<dyn Provider>,
    pub embedder: Arc<dyn polaris_core::conversation_memory::StrictEmbedder>,
    pub database: std::path::PathBuf,
    pub identity: polaris_core::desktop_store::MemoryResources,
    pub embedding_usage: polaris_provider::attempts::AttemptLedger,
}

pub(super) struct TrustedRuns {
    pub configuration: Configuration,
    pub factory: Arc<dyn TrustedRunFactory>,
}

impl DesktopService {
    /// Consume the already validated Writer without reopening or recovering it.
    /// Constructor failure leaves durable artifacts in their existing store.
    pub fn from_trusted_store(
        mut store: Writer,
        runs: Arc<dyn TrustedRunFactory>,
        source: SourceApplyRuntime,
    ) -> Result<Self, ServiceError> {
        let mut saved = store.snapshot()?;
        if store.coordinates() != (&saved.marker.project_id, &saved.marker.session_id) {
            return Err(ServiceError::Options);
        }
        let owner = source_owner::SourceOwner::new(source)?;
        // A configured owner must publish its validated profile before the
        // initial Snapshot, so a new GUI can select workflow behavior. The
        // validator resolves the trusted skill catalog before any run exists.
        // Legacy `None` is also protected by the RunStart preflight.
        if saved.state.workflow.is_none()
            && let Some(config) = runs.workflow_config()
        {
            let workflow = super::validated_workflow(
                config.clone(),
                polaris_core::workflow::WorkflowGatesV1::default(),
                polaris_core::workflow::WorkflowStateV1 {
                    phase: config.initial_phase,
                    ..Default::default()
                },
                saved.state.workflow_artifacts.clone(),
                runs.workflow_skills(),
            )
            .map_err(|_| ServiceError::Options)?;
            store.configure_workflow(saved.marker.session_revision, Some(workflow))?;
            saved = store.snapshot()?;
        }
        let mut service = Self::from_production_store(store, saved.marker.session_id)?;
        let engine = service.engine.as_mut().ok_or(ServiceError::Worker)?;
        engine.trusted_runs = Some(TrustedRuns {
            configuration: saved.state.configuration,
            factory: runs,
        });
        engine.source = Some(owner);
        Ok(service)
    }

    /// Explicit trusted owner entry; intentionally not wired to the binary.
    /// Existing stores must match the supplied immutable configuration exactly.
    /// No source-apply RPC or automatic source apply is available.
    pub fn open_with_trusted_runs(
        config: ServiceConfig,
        configuration: Configuration,
        factory: Arc<dyn TrustedRunFactory>,
    ) -> Result<Self, ServiceError> {
        let mut service = Self::open_storage(config, configuration.clone())?;
        let engine = service.engine.as_mut().ok_or(ServiceError::Worker)?;
        if engine.store.snapshot()?.state.configuration != configuration {
            return Err(ServiceError::Options);
        }
        engine.trusted_runs = Some(TrustedRuns {
            configuration,
            factory,
        });
        Ok(service)
    }

    /// Source runtime is opt-in trusted configuration, never enabled by the binary.
    pub fn open_with_trusted_source_apply(
        config: ServiceConfig,
        configuration: Configuration,
        runs: Arc<dyn TrustedRunFactory>,
        source: SourceApplyRuntime,
    ) -> Result<Self, ServiceError> {
        let owner = source_owner::SourceOwner::new(source)?;
        let mut service = Self::open_with_trusted_runs(config, configuration, runs)?;
        service.engine.as_mut().ok_or(ServiceError::Worker)?.source = Some(owner);
        Ok(service)
    }

    pub fn source_apply_available(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|engine| engine.source_enabled())
    }

    /// Linux frontend entry. Recover the source-bound store before attaching
    /// runtime resources; an unresolved effect retains inspection only.
    #[cfg(target_os = "linux")]
    pub fn open_source_bound_with_trusted_runs(
        config: ServiceConfig,
        source_path: std::path::PathBuf,
        source_identity: (u64, u64),
        configuration: Configuration,
        runs: Arc<dyn TrustedRunFactory>,
        source: SourceApplyRuntime,
    ) -> Result<Self, ServiceError> {
        let owner = source_owner::SourceOwner::new(source)?;
        let mut service = Self::open_storage_bound(
            config,
            configuration.clone(),
            Some((source_path, source_identity)),
        )?;
        let engine = service.engine.as_mut().ok_or(ServiceError::Worker)?;
        let saved = engine.store.snapshot()?;
        if saved.state.configuration != configuration || engine.management_owner.is_none() {
            // Inspect retained results even when startup cannot authorize new
            // work. Do not rewrite workflow or attach a dispatch factory.
            engine.source = Some(owner);
            return Ok(service);
        }
        if saved.state.workflow.is_none()
            && let Some(config) = runs.workflow_config()
        {
            let workflow = super::validated_workflow(
                config.clone(),
                polaris_core::workflow::WorkflowGatesV1::default(),
                polaris_core::workflow::WorkflowStateV1 {
                    phase: config.initial_phase,
                    ..Default::default()
                },
                saved.state.workflow_artifacts.clone(),
                runs.workflow_skills(),
            )
            .map_err(|_| ServiceError::Options)?;
            engine
                .store
                .configure_workflow(saved.marker.session_revision, Some(workflow))?;
        }
        engine.trusted_runs = Some(TrustedRuns {
            configuration,
            factory: runs,
        });
        engine.source = Some(owner);
        Ok(service)
    }
}

enum PrepareOutcome {
    Prepared(Box<TrustedRunInputs>),
    Failed,
    PanicUnknown,
}
#[cfg(test)]
thread_local! { static FAIL_PREPARE_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
struct PrepareJob {
    worker: Option<std::thread::JoinHandle<PrepareOutcome>>,
}
impl PrepareJob {
    fn start(
        target: RunTarget,
        published: Published,
        factory: Arc<dyn TrustedRunFactory>,
    ) -> std::io::Result<Self> {
        #[cfg(test)]
        if FAIL_PREPARE_SPAWN.with(|flag| flag.replace(false)) {
            return Err(std::io::Error::other("injected prepare spawn failure"));
        }
        let worker = std::thread::Builder::new()
            .name("polaris-prepare".into())
            .spawn(move || {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    factory.prepare(&target, &published)
                })) {
                    Ok(Ok(inputs)) => PrepareOutcome::Prepared(Box::new(inputs)),
                    Ok(Err(_)) => PrepareOutcome::Failed,
                    Err(payload) => {
                        // Factory owns cleanup of partial preparation on unwind. A
                        // panic is not proof of no side effects or a usable copy.
                        std::mem::forget(payload);
                        PrepareOutcome::PanicUnknown
                    }
                }
            })?;
        Ok(Self {
            worker: Some(worker),
        })
    }
    fn poll(&mut self) -> Option<PrepareOutcome> {
        if !self.worker.as_ref()?.is_finished() {
            return None;
        }
        Some(match self.worker.take().expect("finished worker").join() {
            Ok(outcome) => outcome,
            Err(payload) => {
                std::mem::forget(payload);
                PrepareOutcome::PanicUnknown
            }
        })
    }
}
impl Drop for PrepareJob {
    fn drop(&mut self) {
        // Emergency blocking-owner destruction only. Normal loops poll and
        // retain returned inputs. Never detach a preparation thread.
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            std::mem::forget(payload);
        }
    }
}
enum Preparation {
    Unstarted,
    Running(PrepareJob),
    Joined,
    SpawnFailed,
}

pub(super) struct RealRun {
    preparation: Preparation,
    prepared_inputs: Option<TrustedRunInputs>,
    prepare_policy_revision: Option<DecimalU64>,
    cancelled: bool,
    worker: Option<DesktopRun>,
    events: Option<DesktopEventReceiver>,
    memory: Option<polaris_core::desktop_memory::DesktopMemoryOwner>,
    workflow_owner: Option<polaris_core::desktop_workflow::DesktopWorkflowOwner>,
    summary_usage: Option<polaris_provider::UsageMeter>,
    main_usage: polaris_provider::UsageMeter,
    embedding_usage: Option<polaris_provider::attempts::AttemptLedger>,
    last_memory_event: Option<polaris_desktop_protocol::snapshot::MemoryStatus>,
    // Retain the sanitized copy through core/native cleanup and durable finish.
    _prepared: Option<Arc<PreparedWorkspace>>,
    prefix: Vec<Message>,
    suffix: Vec<Message>,
    outcome: Option<Observation>,
    workflow: Option<SavedWorkflow>,
    output_rejected: bool,
    memory_evidence_over_budget: bool,
    joined: bool,
    sequence: u64,
    /// M6-2: the work in flight and when it started, from the run's own
    /// events. Silence changes nothing here.
    work: Option<(polaris_core::activity::Work, std::time::Instant)>,
    judging_depth: usize,
    activity: Option<polaris_core::activity::Activity>,
}
impl RealRun {
    pub(super) fn queued() -> Self {
        Self {
            preparation: Preparation::Unstarted,
            prepared_inputs: None,
            prepare_policy_revision: None,
            cancelled: false,
            worker: None,
            events: None,
            memory: None,
            workflow_owner: None,
            summary_usage: None,
            main_usage: Default::default(),
            embedding_usage: None,
            last_memory_event: None,
            _prepared: None,
            prefix: vec![],
            suffix: vec![],
            outcome: None,
            workflow: None,
            output_rejected: false,
            memory_evidence_over_budget: false,
            joined: false,
            sequence: 0,
            work: None,
            judging_depth: 0,
            activity: None,
        }
    }

    /// Only a start and its matching end move this; a quiet period does not.
    fn observe_activity(
        &mut self,
        event: &AgentEvent,
    ) -> Result<Option<polaris_desktop_protocol::event::RunActivityPhase>, ServiceError> {
        use polaris_core::activity::Work;
        use polaris_desktop_protocol::event::RunActivityPhase;
        let decision = match event {
            AgentEvent::DecisionStarted => {
                let was_idle = self.judging_depth == 0;
                self.judging_depth = self
                    .judging_depth
                    .checked_add(1)
                    .ok_or(ServiceError::Worker)?;
                was_idle.then_some(RunActivityPhase::Judging)
            }
            AgentEvent::DecisionFinished => {
                self.judging_depth = self
                    .judging_depth
                    .checked_sub(1)
                    .ok_or(ServiceError::Worker)?;
                (self.judging_depth == 0).then_some(RunActivityPhase::Working)
            }
            _ => None,
        };
        self.work = match event {
            AgentEvent::ModelRequestStarted { .. } => {
                Some((Work::ModelRequest, std::time::Instant::now()))
            }
            AgentEvent::ToolStarted { .. } => Some((Work::Tool, std::time::Instant::now())),
            AgentEvent::SpawnStarted { .. } => Some((Work::Child, std::time::Instant::now())),
            AgentEvent::ModelRequestFinished { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::SpawnFinished { .. } => None,
            _ => return Ok(decision),
        };
        Ok(decision)
    }

    fn refresh_activity(&mut self, awaiting_approval: bool) {
        use polaris_core::activity::{Deadlines, Observed, assess};
        let observed = Observed {
            finished: self
                .joined
                .then_some(!matches!(self.outcome, Some(Observation::Failed))),
            awaiting_approval,
            awaiting_input: false,
            awaiting_dependency: false,
            work: match (&self.preparation, self.work) {
                (Preparation::Running(_), _) => Some((
                    polaris_core::activity::Work::Preparation,
                    std::time::Duration::ZERO,
                )),
                (_, Some((work, since))) => Some((work, since.elapsed())),
                _ => None,
            },
        };
        self.activity = Some(assess(&observed, &Deadlines::default()));
    }
    pub(super) fn cancel(&mut self) {
        self.cancelled = true;
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
    }
    /// Drain a finite batch, retaining the receiver until it reports disconnection.
    /// With failed storage, the owner still drains and joins, but publishes nothing.
    pub(super) fn poll(
        &mut self,
        target: &RunTarget,
        mut store: Option<&mut Writer>,
    ) -> Result<Vec<EventBody>, ServiceError> {
        if let Some(memory) = &mut self.memory {
            memory.poll(store.as_deref_mut(), self.cancelled);
        }
        if let Some(workflow) = &mut self.workflow_owner {
            workflow.poll(store.as_deref_mut(), self.cancelled, self.activity);
        }
        if let Preparation::Running(job) = &mut self.preparation
            && let Some(outcome) = job.poll()
        {
            self.preparation = Preparation::Joined;
            match outcome {
                PrepareOutcome::Prepared(inputs) => {
                    self._prepared = Some(inputs.prepared.clone());
                    self.prepared_inputs = Some(*inputs);
                }
                PrepareOutcome::Failed => self.outcome = Some(Observation::Failed),
                PrepareOutcome::PanicUnknown => self.outcome = Some(Observation::OutcomeUnknown),
            }
        }
        let mut updates = Vec::new();
        for _ in 0..EVENT_CAPACITY {
            let Some(events) = &mut self.events else {
                break;
            };
            let event = match events.try_recv() {
                Ok(event) => event,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.events = None;
                    break;
                }
            };
            if event.run_id != target.run_id || self.sequence.checked_add(1) != Some(event.sequence)
            {
                self.cancel();
                return Err(ServiceError::Worker);
            }
            self.sequence = event.sequence;
            if let Some(phase) = self.observe_activity(&event.event)? {
                updates.push(EventBody::RunActivity(
                    polaris_desktop_protocol::event::RunActivity {
                        run_id: target.run_id.clone(),
                        attempt_id: target.attempt_id.clone(),
                        phase,
                    },
                ));
            }
            if let Some(store) = store.as_deref_mut() {
                match persist_child_event(store, target, event) {
                    Ok(Some(update)) => updates.push(update),
                    Ok(None) => {}
                    Err(error) => {
                        self.cancel();
                        return Err(error);
                    }
                }
            }
        }
        let Some(joined) = self.worker.as_mut().and_then(DesktopRun::try_join) else {
            return Ok(updates);
        };
        self.worker = None;
        self.joined = true;
        match joined {
            Ok(completion) => {
                self.output_rejected = completion.session.persistence_error.is_some();
                // Only a typed local failure may add detail; provider errors stay private.
                self.memory_evidence_over_budget = matches!(
                    &completion.result,
                    Err(DesktopRunError::Agent(AgentError::Io(error)))
                        if matches!(
                            error.get_ref().and_then(|cause| cause.downcast_ref::<polaris_core::conversation_memory::ConversationMemoryError>()),
                            Some(polaris_core::conversation_memory::ConversationMemoryError::ConversationEvidenceBudgetExceeded)
                        )
                );
                if completion.session.messages.len() < self.prefix.len()
                    || !completion
                        .session
                        .messages
                        .iter()
                        .zip(&self.prefix)
                        .all(|(a, b)| same_message(a, b))
                {
                    return Err(StoreError::RecoveryRequired.into());
                }
                self.workflow = completion
                    .session
                    .workflow
                    .as_ref()
                    .map(SavedWorkflow::capture);
                self.suffix = completion.turn_suffix;
                self.outcome = Some(match completion.result {
                    Ok(_) if !self.output_rejected => Observation::Succeeded,
                    Ok(_) => Observation::Failed,
                    Err(DesktopRunError::Agent(AgentError::Desktop(EventFailure::Cancelled))) => {
                        Observation::Cancelled
                    }
                    Err(_) => Observation::Failed,
                });
            }
            // An outer panic loses the transcript; never manufacture success.
            Err(_) => self.outcome = Some(Observation::OutcomeUnknown),
        }
        Ok(updates)
    }

    fn prepare_memory(
        &mut self,
        history: Option<TrustedHistoryResources>,
        target: &RunTarget,
        store: &mut Writer,
        total: &polaris_provider::UsageMeter,
        judge: Option<Arc<dyn polaris_provider::decision::Decider>>,
    ) -> Result<Option<Arc<polaris_core::desktop_memory::DesktopMemory>>, ServiceError> {
        let expected = store
            .snapshot()?
            .state
            .runs
            .iter()
            .find(|r| r.run.run_id == target.run_id)
            .ok_or(StoreError::NotFound)?
            .configuration
            .history_mode;
        if history.is_some() == expected.is_legacy() {
            return Err(ServiceError::Options);
        }
        let Some(history) = history else {
            return Ok(None);
        };
        let summary = Arc::new(polaris_core::strict_provider::ProviderSummary::new(
            Arc::new(total.wrap(history.summary_provider)),
        ));
        let summary_usage = summary.usage.clone();
        // The same judge the run's preselection uses. Here it only reorders
        // the related candidates; it never drops one.
        let strict = Arc::new(
            polaris_core::conversation_memory::StrictHistory::new(summary, history.embedder)
                .judged_by(judge),
        );
        let (memory, owner) = polaris_core::desktop_memory::DesktopMemory::pair(
            target.clone(),
            strict,
            history.database.clone(),
            history.identity.clone(),
        )
        .map_err(|_| ServiceError::Options)?;
        if expected.is_structured() {
            store.prepare_structured_memory_index(target, &history.database, &history.identity)?;
        }
        store.bind_memory(target, history.identity)?;
        self.summary_usage = Some(summary_usage);
        self.embedding_usage = Some(history.embedding_usage);
        self.memory = Some(owner);
        Ok(Some(memory))
    }

    fn checkpoint_memory(
        &mut self,
        target: &RunTarget,
        store: &mut Writer,
        total: &polaris_provider::UsageMeter,
    ) -> Result<Option<EventBody>, ServiceError> {
        use polaris_desktop_protocol::snapshot::{EmbeddingUsage, MemoryPhase};
        use polaris_provider::attempts::{AttemptStatus, UsageObservation};
        let Some(mut status) = store
            .snapshot()?
            .state
            .runs
            .iter()
            .find(|r| r.run.run_id == target.run_id)
            .and_then(|r| r.memory.clone())
        else {
            return Ok(None);
        };
        status.total_usage = observed_usage(total.snapshot());
        status.main_usage = observed_usage(self.main_usage.snapshot());
        status.summary_usage = self
            .summary_usage
            .as_ref()
            .and_then(|m| observed_usage(m.snapshot()));
        if let Some(ledger) = &self.embedding_usage {
            let records = ledger.snapshot();
            if !records.is_empty() {
                let mut usage = EmbeddingUsage {
                    requests: DecimalU64::new(records.len() as u64),
                    ..Default::default()
                };
                for record in records {
                    match record.status {
                        AttemptStatus::Succeeded => {
                            usage.completed =
                                usage.completed.checked_add(1).map_err(StoreError::from)?
                        }
                        AttemptStatus::Failed => {
                            usage.failed = usage.failed.checked_add(1).map_err(StoreError::from)?
                        }
                        AttemptStatus::Running | AttemptStatus::Cancelled => {
                            usage.unknown =
                                usage.unknown.checked_add(1).map_err(StoreError::from)?
                        }
                    }
                    if let UsageObservation::Known { input_tokens, .. } = record.usage {
                        usage.input_tokens = usage
                            .input_tokens
                            .checked_add(u64::from(input_tokens))
                            .map_err(StoreError::from)?;
                    }
                }
                status.embedding_usage = Some(usage);
            }
        }
        if let Some(outcome) = self.outcome {
            if outcome == Observation::OutcomeUnknown {
                status.phase = MemoryPhase::OutcomeUnknown;
                status.detail = "実行結果が不明です。要約を自動再送していません".into();
            } else if status.phase == MemoryPhase::Preparing {
                status.phase = MemoryPhase::Failed;
                status.detail =
                    "会話記憶の準備を完了できなかったため主要求を開始していません".into();
            }
            if self.memory_evidence_over_budget {
                status.detail = "出典情報を含む記憶が256トークンを超えたため主要求を開始していません。要約は自動再送していません".into();
            }
        }
        store.record_memory_status(target, status.clone())?;
        if self.last_memory_event.as_ref() == Some(&status) {
            return Ok(None);
        }
        self.last_memory_event = Some(status.clone());
        Ok(Some(EventBody::MemoryUpdated(status)))
    }
}

fn observed_usage(
    report: polaris_provider::UsageReport,
) -> Option<polaris_desktop_protocol::snapshot::MemoryUsage> {
    (report.reported_responses + report.missing_responses + report.failed_requests > 0).then(|| {
        polaris_desktop_protocol::snapshot::MemoryUsage {
            input_tokens: DecimalU64::new(u64::from(report.usage.input_tokens)),
            output_tokens: DecimalU64::new(u64::from(report.usage.output_tokens)),
            cached_tokens: DecimalU64::new(u64::from(report.usage.cached_tokens)),
            reported_responses: DecimalU64::new(report.reported_responses),
            missing_responses: DecimalU64::new(report.missing_responses),
            failed_requests: DecimalU64::new(report.failed_requests),
        }
    })
}

/// Translate only stable child lifecycle events; persist before any UI exposure.
fn persist_child_event(
    store: &mut Writer,
    target: &RunTarget,
    envelope: EventEnvelope,
) -> Result<Option<EventBody>, ServiceError> {
    let identity = match envelope.child {
        Some(identity) => identity,
        None if matches!(
            envelope.event,
            AgentEvent::SpawnStarted { .. } | AgentEvent::SpawnFinished { .. }
        ) =>
        {
            return Err(ServiceError::Worker);
        }
        None => return Ok(None),
    };
    let receipt = match envelope.event {
        AgentEvent::SpawnStarted {
            agent_type, task, ..
        } => {
            let published = store.snapshot()?;
            let parent_tasks = if identity.parent_run_id == target.run_id {
                &published
                    .state
                    .runs
                    .iter()
                    .find(|r| {
                        r.run.run_id == target.run_id && r.run.attempt_id == target.attempt_id
                    })
                    .ok_or(StoreError::RunConflict)?
                    .run
                    .task_ids
            } else {
                &published
                    .state
                    .children
                    .iter()
                    .find(|c| c.root == *target && c.child.run_id == identity.parent_run_id)
                    .ok_or(StoreError::RunConflict)?
                    .child
                    .task_ids
            };
            store.record_child_start(
                target,
                polaris_desktop_protocol::snapshot::Child {
                    run_id: identity.run_id.clone(),
                    attempt_id: identity.attempt_id.clone(),
                    parent_run_id: identity.parent_run_id.clone(),
                    display_name: None,
                    state: RunState::Running,
                    task_ids: parent_tasks.clone(),
                },
                agent_type,
                task,
            )?
        }
        AgentEvent::SpawnFinished { agent_type, ok, .. } => {
            let published = store.snapshot()?;
            let child = published
                .state
                .children
                .iter()
                .find(|c| c.root == *target && c.child.run_id == identity.run_id)
                .ok_or(StoreError::RunConflict)?;
            if child.agent_type != agent_type {
                return Err(StoreError::RunConflict.into());
            }
            store.record_child_finish(
                target,
                &identity,
                if ok {
                    Observation::Succeeded
                } else {
                    Observation::Failed
                },
            )?
        }
        _ => return Ok(None),
    };
    if receipt == IntentReceipt::AlreadyRecorded {
        return Ok(None);
    }
    let child = store
        .snapshot()?
        .state
        .children
        .into_iter()
        .find(|c| c.root == *target && c.child.run_id == identity.run_id)
        .ok_or(StoreError::RunConflict)?
        .child;
    Ok(Some(EventBody::ChildUpdated(child)))
}

/// Validate the core's deterministic request-only projection. The worker applies
/// it before inference; this owner retains the original durable raw prefix.
/// Output gaps still require explicit recovery, not an invented tool outcome.
pub(super) fn history_ready(published: &Published) -> bool {
    if published
        .state
        .runs
        .iter()
        .any(|run| run.history_gap.is_some())
    {
        return false;
    }
    let raw: Vec<_> = published
        .raw
        .iter()
        .map(|raw| raw.message.clone())
        .collect();
    polaris_core::session::desktop_request_history(&raw).is_ok()
}

fn same_message(a: &Message, b: &Message) -> bool {
    a.role == b.role
        && a.content == b.content
        && a.tool_call_id == b.tool_call_id
        && a.reasoning == b.reasoning
        && a.hosted_web_search == b.hosted_web_search
        && a.url_citations == b.url_citations
        && a.tool_calls.len() == b.tool_calls.len()
        && a.tool_calls
            .iter()
            .zip(&b.tool_calls)
            .all(|(a, b)| a.id == b.id && a.name == b.name && a.arguments == b.arguments)
}

impl Engine {
    pub(super) fn step_real(
        &mut self,
        out: Option<&Outbox>,
        stopping: bool,
    ) -> Result<(), ServiceError> {
        if self.failed {
            self.cancel_and_poll_core();
            return Err(StoreError::RecoveryRequired.into());
        }
        let published = self.store.snapshot()?;
        // Taken before `active` is borrowed, because the request context
        // below is built inside that borrow. Leaving it `None` there is what
        // kept the desktop on the judge-free path: the owner was started with
        // a judge and then never handed it to preselection.
        let judge = self
            .trusted_runs
            .as_ref()
            .and_then(|trusted| trusted.factory.decider());
        let delegation_root = self.root.clone();
        let delegation_project = self.project.clone();
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        let state = published
            .state
            .runs
            .iter()
            .find(|r| {
                r.run.run_id == active.target.run_id && r.run.attempt_id == active.target.attempt_id
            })
            .ok_or(StoreError::NotFound)?;
        let cancel = stopping
            || self.disconnected.load(Ordering::Acquire)
            || state.run.state == RunState::Cancelling
            || state.run.state.is_terminal();
        if cancel {
            active.cancel();
        }
        let real = active.real.as_mut().ok_or(ServiceError::Worker)?;
        if !active.started && real.outcome.is_none() {
            if cancel {
                real.outcome = Some(Observation::Cancelled);
            } else {
                // Acceptance and intent are separate durable boundaries. A saved
                // intent is never replayed after a failed start or reconnect.
                if self
                    .store
                    .record_intent(&active.target, active.operation.clone())?
                    != IntentReceipt::NewlyPublished
                {
                    return Err(StoreError::RunConflict.into());
                }
                active.started = true;
                real.prefix = self.store.raw_request_prefix(&active.target)?;
                real.workflow = state.workflow.clone();
                let trusted = self.trusted_runs.as_ref().ok_or(ServiceError::Options)?;
                real.prepare_policy_revision = Some(published.state.policy_revision);
                if state.configuration != trusted.configuration {
                    real.outcome = Some(Observation::Failed);
                } else {
                    match PrepareJob::start(
                        active.target.clone(),
                        self.store.snapshot()?,
                        trusted.factory.clone(),
                    ) {
                        Ok(job) => real.preparation = Preparation::Running(job),
                        Err(_) => {
                            real.preparation = Preparation::SpawnFailed;
                            real.outcome = Some(Observation::Failed);
                        }
                    }
                }
                if let Some(out) = out {
                    let run = self
                        .store
                        .snapshot()?
                        .state
                        .runs
                        .iter()
                        .find(|r| r.run.run_id == active.target.run_id)
                        .ok_or(StoreError::NotFound)?
                        .run
                        .clone();
                    self.emit(EventBody::RunState(run), out)?;
                }
            }
        }
        let input_preparation = self
            .trusted_runs
            .as_ref()
            .map(|trusted| trusted.factory.input_preparation())
            .unwrap_or_default();
        let active = self.active.as_mut().ok_or(ServiceError::Worker)?;
        let awaiting_approval = active.execution.awaiting_approval();
        let real = active.real.as_mut().ok_or(ServiceError::Worker)?;
        // The window reports what the run is doing; it is read, not acted on.
        real.refresh_activity(awaiting_approval);
        // Approval waits must never suppress this poll or cancellation delivery.
        let child_events = match real.poll(&active.target, Some(&mut self.store)) {
            Ok(events) => events,
            Err(error) => {
                active.cancel();
                self.failed = true;
                return Err(error);
            }
        };
        // poll first retains returned inputs. From this point to start there is
        // no await or owner yield; cancellation is latched across failed storage.
        if real.prepared_inputs.is_some() && real.outcome.is_none() {
            let current = self.store.snapshot()?;
            let target = current
                .state
                .runs
                .iter()
                .find(|r| {
                    r.run.run_id == active.target.run_id
                        && r.run.attempt_id == active.target.attempt_id
                })
                .ok_or(StoreError::NotFound)?;
            let revoked = real.prepare_policy_revision != Some(current.state.policy_revision)
                || target.policy_revision != current.state.policy_revision
                || target.configuration != current.state.configuration
                || self
                    .trusted_runs
                    .as_ref()
                    .is_none_or(|t| target.configuration != t.configuration);
            if real.cancelled
                || stopping
                || self.draining
                || self.disconnected.load(Ordering::Acquire)
                || target.run.state == RunState::Cancelling
                || target.run.state.is_terminal()
            {
                real.outcome = Some(Observation::Cancelled);
            } else if revoked {
                real.outcome = Some(Observation::Failed);
            } else {
                let inputs = real.prepared_inputs.as_ref().expect("retained inputs");
                let execution =
                    crate::execution::RunExecution::with_prepared_workspace_and_capabilities(
                        &active.target,
                        self.disconnected.clone(),
                        inputs.prepared.clone(),
                        inputs.mutation_policy.clone(),
                        inputs.execution_capability.clone(),
                    );
                if let Ok(execution) = execution {
                    active.execution = execution;
                    let workflow = real
                        .workflow
                        .as_ref()
                        .map(|saved| {
                            saved.restore_with_artifacts(
                                None,
                                current.state.workflow_artifacts.clone(),
                            )
                        })
                        .transpose()?;
                    if self.disconnected.load(Ordering::Acquire) {
                        real.cancelled = true;
                        real.outcome = Some(Observation::Cancelled);
                        active.execution.cancel();
                    } else {
                        let sources =
                            polaris_core::decision_capacity::colocation::ColocationSources::new(
                                Vec::new(),
                            );
                        let mut colocated =
                            vec![inputs.provider.clone(), inputs.provider_pool.clone()];
                        if let Some(history) = &inputs.history {
                            colocated.push(history.summary_provider.clone());
                        }
                        if let Some(selection) = inputs
                            .mcp
                            .as_ref()
                            .and_then(|mcp| mcp.selection_provider.clone())
                        {
                            colocated.push(selection);
                        }
                        sources.replace(
                            colocated,
                            inputs.history.is_some() || inputs.tool_memory.is_some(),
                        );
                        let judge = judge.clone().map(|decider| sources.bind(decider));
                        let (event_sink, event_receiver, _event_control) =
                            DesktopEventSink::channel(active.target.run_id.clone());
                        let observed_judge = judge
                            .clone()
                            .map(|decider| event_sink.observe_decider(decider));
                        let mut inputs = real
                            .prepared_inputs
                            .take()
                            .expect("validated retained inputs");
                        let has_mcp = inputs.mcp.is_some();
                        let desktop_memory = real.prepare_memory(
                            inputs.history.take(),
                            &active.target,
                            &mut self.store,
                            &active.usage,
                            observed_judge.clone(),
                        );
                        if desktop_memory.is_err() {
                            real.outcome = Some(Observation::Failed);
                        }
                        let desktop_workflow = workflow.as_ref().map(|_| {
                            let (workflow, owner) =
                                polaris_core::desktop_workflow::DesktopWorkflow::pair(
                                    active.target.clone(),
                                );
                            real.workflow_owner = Some(owner);
                            workflow
                        });
                        let request_context = if let Some(mcp) = inputs.mcp.take() {
                            if mcp.config.project_id() != current.marker.project_id.as_str() {
                                Err(StoreError::TargetMismatch)
                            } else {
                                active
                                    .execution
                                    .configure_mcp(
                                        &active.target,
                                        current.marker.session_id.clone(),
                                        inputs.external_ceiling,
                                        polaris_core::approval::ApprovalPolicy::Always,
                                    )
                                    .map(|port| {
                                        // Read at root run start; a busy or damaged
                                        // cache only disables preselection.
                                        #[cfg(target_os = "macos")]
                                        let index = self.mcp_settings.as_ref().map_or(
                                            Ok(None),
                                            |settings| {
                                                settings
                                                    .store
                                                    .load_catalog_index()
                                                    .map(|index| {
                                                        index.map(|index| {
                                                            index.verified(mcp.config.registry())
                                                        })
                                                    })
                                                    .map_err(polaris_core::mcp::preselection::index_status)
                                            },
                                        );
                                        #[cfg(target_os = "linux")]
                                        let index = Ok(None);
                                        let selector = mcp.selection_provider.map(|provider| {
                                            Arc::new(active.usage.wrap(provider))
                                                as Arc<dyn Provider>
                                        });
                                        DesktopRequestContext::Mcp {
                                            config: Box::new(mcp.config),
                                            port,
                                            prompt: target.mcp_prompt.clone(),
                                            preselection:
                                                polaris_core::desktop_run::McpPreselection {
                                                    index,
                                                    selector,
                                                    decider: observed_judge.clone(),
                                                },
                                        }
                                    })
                            }
                        } else {
                            self.store
                                .project_request_history(&active.target)
                                .map(DesktopRequestContext::Prepared)
                        };
                        // Detection and approved registration only; a store
                        // failure leaves the run without the tool.
                        #[cfg(target_os = "macos")]
                        let mcp_setup = self.mcp_settings.as_ref().and_then(|settings| {
                            let port = active
                                .execution
                                .configure_registration(&active.target, inputs.external_ceiling)
                                .ok()?;
                            Some(Arc::new(polaris_core::mcp::setup::SetupTool::new(
                                settings.home.clone(),
                                settings.runtime_roots.clone(),
                                settings.excluded_roots.clone(),
                                settings.limits,
                                current.marker.project_id.as_str().into(),
                                polaris_core::mcp::setup::Authorization::Owner(port),
                            )))
                        });
                        #[cfg(target_os = "linux")]
                        let mcp_setup = None;
                        if let Ok(request_context) = request_context {
                            let delegation = create_delegation_runtime(
                                delegation_root.as_ref().ok_or(ServiceError::Worker)?,
                                &delegation_project,
                                &current,
                                &active.target,
                                &target.configuration,
                                workflow.as_ref(),
                                &inputs.prepared,
                                has_mcp,
                            )?;
                            let mut session = Session {
                                messages: real.prefix.clone(),
                                tool_memory: inputs.tool_memory,
                                workflow,
                                desktop_memory: desktop_memory.unwrap_or(None),
                                desktop_workflow,
                                mcp_setup,
                                decider: judge.clone(),
                                input_preparation,
                                ..Session::new()
                            };
                            session
                                .install_root_delegation_control(delegation.control.clone())
                                .map_err(|_| ServiceError::Worker)?;
                            active.delegation = Some(delegation);
                            let input =
                                DesktopRunInput {
                                    target: active.target.clone(),
                                    prepared: inputs.prepared,
                                    execution: active.execution.port.clone(),
                                    provider: Arc::new(
                                        active
                                            .usage
                                            .wrap(Arc::new(real.main_usage.wrap(inputs.provider))),
                                    ),
                                    provider_pool: Arc::new(active.usage.wrap(Arc::new(
                                        real.main_usage.wrap(inputs.provider_pool),
                                    ))),
                                    session,
                                    request_context,
                                    always_on: inputs.always_on,
                                    audit: inputs.audit,
                                    max_turns: inputs.max_turns,
                                    skills: inputs.skills,
                                    agent_types: inputs.agent_types,
                                    spawn_concurrency: inputs.spawn_concurrency,
                                    spawn_write_concurrency: inputs.spawn_write_concurrency,
                                };
                            if real.outcome.is_none() {
                                match DesktopRun::start_with_events(
                                    input,
                                    event_sink,
                                    event_receiver,
                                ) {
                                    Ok((worker, events)) => {
                                        real.worker = Some(worker);
                                        real.events = Some(events);
                                    }
                                    Err(_) => real.outcome = Some(Observation::Failed),
                                }
                            }
                        } else {
                            real.outcome = Some(Observation::Failed);
                        }
                    }
                } else {
                    real.outcome = Some(Observation::Failed);
                }
            }
        }
        if real.outcome.is_some() {
            active
                .execution
                .record_finished_registrations(&mut self.store, &active.target)?;
            // An unresolved summary may have consumed a provider request. Preserve
            // its durable intent and refuse automatic replay after restart.
            if self
                .store
                .snapshot()?
                .state
                .runs
                .iter()
                .find(|r| r.run.run_id == active.target.run_id)
                .is_some_and(|r| {
                    r.operations
                        .iter()
                        .any(|op| op.operation_id != active.operation && op.result_id.is_none())
                })
            {
                real.outcome = Some(Observation::OutcomeUnknown);
            }
            active.execution.cancel();
        }
        let memory_event =
            real.checkpoint_memory(&active.target, &mut self.store, &active.usage)?;
        active.execution.step(&mut self.store, &active.target)?;
        let approval_events = active.execution.take_approval_events();
        self.cleanups.step();
        self.checkpoint_usage()?;
        if let Some(out) = out {
            self.emit_all(child_events, out)?;
            self.emit_all(approval_events, out)?;
            if let Some(event) = memory_event {
                self.emit(event, out)?;
            }
        }
        let active = self.active.as_mut().ok_or(ServiceError::Worker)?;
        let real = active.real.as_mut().ok_or(ServiceError::Worker)?;
        let Some(outcome) = real.outcome else {
            return Ok(());
        };
        if matches!(real.preparation, Preparation::Running(_))
            || real.worker.is_some()
            || real.events.is_some()
            || !active.execution.quiescent()
            || !self.cleanups.is_empty()
        {
            return Ok(());
        }
        let outcome = active.execution.observation(outcome);
        let result = ResultId::new(format!("result-{}", active.target.run_id.as_str()))
            .map_err(|_| ServiceError::Worker)?;
        let unfinished_children: Vec<_> = self
            .store
            .snapshot()?
            .state
            .children
            .into_iter()
            .filter(|c| c.root == active.target && !c.child.state.is_terminal())
            .map(|c| c.child.run_id)
            .collect();
        if active.started {
            // MCP may have published its canonical result after the caller
            // future was dropped. Reconcile only after owner cleanup completes.
            let checkpointed_workflow = self
                .store
                .snapshot()?
                .state
                .runs
                .iter()
                .find(|record| {
                    record.run.run_id == active.target.run_id
                        && record.run.attempt_id == active.target.attempt_id
                })
                .ok_or(StoreError::NotFound)?
                .workflow
                .clone();
            let suffix = self.store.reconcile_mcp_turn_suffix(
                &active.target,
                real.suffix.clone(),
                checkpointed_workflow.as_ref(),
            )?;
            if real.output_rejected {
                self.store
                    .record_history_gap(&active.target, HistoryGap::OutputRejected)
                    .map_err(|_| StoreError::RecoveryRequired)?;
            }
            self.store.finish_with_messages(
                &active.target,
                &active.operation,
                outcome,
                result,
                suffix,
            )?;
        } else if !state.run.state.is_terminal() {
            self.store
                .finish(&active.target, Observation::Cancelled, result)?;
        }
        active.execution.saved = true;
        if let Some(delegation) = &active.delegation {
            revoke_root_delegation(delegation.control.clone())?;
        }
        // A cancelled run can still be saved as failed (its last checkpoint is
        // refused), so the continuation is told about the cancel directly.
        let stopped_by_person = cancel || active.real.as_ref().is_some_and(|real| real.cancelled);
        let admission_terminal = active.admission.clone().map(|id| {
            let status = if !active.started || outcome == Observation::Cancelled {
                polaris_core::session_manager::TerminalStatus::Cancelled
            } else if outcome == Observation::Succeeded {
                polaris_core::session_manager::TerminalStatus::Completed
            } else if outcome == Observation::OutcomeUnknown {
                polaris_core::session_manager::TerminalStatus::OutcomeUnknown
            } else {
                polaris_core::session_manager::TerminalStatus::Failed
            };
            (id, status)
        });
        if let (Some(owner), Some((admission, status))) =
            (self.admission.as_mut(), admission_terminal)
        {
            owner.terminal(&admission, status)?;
            if let Some(out) = out {
                let published = self.store.snapshot()?;
                self.emit(
                    EventBody::OrchestrationUpdated(super::orchestration_summary(
                        &published,
                        self.admission.as_ref(),
                        self.delegation_projection().as_ref(),
                    )),
                    out,
                )?;
            }
        }
        let active = self.active.take().ok_or(ServiceError::Worker)?;
        let saved = self.store.snapshot()?;
        if let Some(prepared) = active
            .real
            .as_ref()
            .filter(|r| r.joined)
            .and_then(|r| r._prepared.clone())
        {
            let terminal = saved
                .state
                .runs
                .iter()
                .find(|r| {
                    r.run.run_id == active.target.run_id
                        && r.run.attempt_id == active.target.attempt_id
                })
                .filter(|r| r.run.state.is_terminal())
                .ok_or(ServiceError::Worker)?
                .clone();
            let receipt = TrustedRunCompletion {
                project_id: saved.marker.project_id.clone(),
                session_id: saved.marker.session_id.clone(),
                target: active.target.clone(),
                prepared,
                terminal,
                session_revision: saved.marker.session_revision,
            };
            if let Some(source) = &mut self.source {
                source.completed(receipt);
            } else {
                self.trusted_runs
                    .as_ref()
                    .ok_or(ServiceError::Worker)?
                    .factory
                    .completed(receipt);
            }
        } else if let Some(source) = &mut self.source {
            // A queued cancellation never prepared a copy or produced a receipt.
            source.release_unused(&active.target);
        }
        if let Some(out) = out {
            for child in &saved.state.children {
                if child.root == active.target && unfinished_children.contains(&child.child.run_id)
                {
                    self.emit(EventBody::ChildUpdated(child.child.clone()), out)?;
                }
            }
            // Final delivery comes from committed raw entries, never joined text
            // that failed persistence. Use UTF-8-safe <=32KiB chunks per event.
            if active.started {
                let prefix = active
                    .real
                    .as_ref()
                    .ok_or(ServiceError::Worker)?
                    .prefix
                    .len();
                for raw in saved.raw.iter().skip(prefix) {
                    if raw.message.role != Role::Assistant {
                        continue;
                    }
                    let text = &raw.message.content;
                    let mut offset = 0;
                    loop {
                        let mut end = (offset + TEXT_BYTES).min(text.len());
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        self.emit(
                            EventBody::MessageDelta(TextDelta {
                                message_id: message_id(raw.sequence),
                                byte_offset: DecimalU64::new(offset as u64),
                                text: text[offset..end].into(),
                                durability: Durability::Saved,
                            }),
                            out,
                        )?;
                        if end == text.len() {
                            break;
                        }
                        offset = end;
                    }
                }
            }
            let run = saved
                .state
                .runs
                .into_iter()
                .find(|r| r.run.run_id == active.target.run_id)
                .ok_or(StoreError::NotFound)?
                .run;
            self.emit(EventBody::RunState(run), out)?;
        }
        // Start the non-authoritative copy only after the canonical assistant
        // bytes are durable and their saved deltas have been handed to the UI.
        // Shadow failure cannot change either output or run state.
        if out.is_some()
            && active.started
            && outcome == Observation::Succeeded
            && let Some(finalizer) = self
                .trusted_runs
                .as_ref()
                .and_then(|trusted| trusted.factory.answer_finalizer())
            && let Some(raw) = saved
                .raw
                .iter()
                .skip(
                    active
                        .real
                        .as_ref()
                        .ok_or(ServiceError::Worker)?
                        .prefix
                        .len(),
                )
                .rev()
                .find(|raw| raw.message.role == Role::Assistant)
        {
            let prompt = saved
                .raw
                .iter()
                .rev()
                .find(|entry| entry.sequence < raw.sequence && entry.message.role == Role::User)
                .map(|entry| (entry.sequence, entry.message.content.clone()));
            let (prompt_sequence, prompt) = prompt.unwrap_or((0, String::new()));
            let turn_entries = saved
                .raw
                .iter()
                .filter(|entry| entry.sequence > prompt_sequence && entry.sequence < raw.sequence);
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
            let workflow_revision = active
                .real
                .as_ref()
                .and_then(|real| real.workflow.as_ref())
                .map(|workflow| workflow.state.revision.to_string())
                .unwrap_or_else(|| "0".into());
            let subject = polaris_core::answer_finalizer::FinalizationSubjectV1 {
                schema_version: polaris_core::answer_finalizer::FINALIZATION_SCHEMA_VERSION,
                project_id: saved.marker.project_id.as_str().into(),
                session_id: saved.marker.session_id.as_str().into(),
                session_epoch: saved.marker.epoch.get(),
                lane_id: saved.marker.session_id.as_str().into(),
                root_run_id: active.target.run_id.as_str().into(),
                attempt_id: active.target.attempt_id.as_str().into(),
                turn_sequence: raw.sequence,
                input_receipt_ids: vec![active.message.as_str().into()],
                attachment_hashes: vec![],
                workflow_revision,
                exact_prompt_hash: polaris_core::conversation_state::content_hash(
                    prompt.as_bytes(),
                ),
            };
            let _ = finalizer.schedule(crate::shadow_finalizer::ShadowInputV1 {
                subject,
                source_message_id: message_id(raw.sequence).as_str().into(),
                prompt,
                source_body: raw.message.content.clone(),
                closed_evidence,
                evidence_complete,
            });
        }
        self.continue_goal(out, stopped_by_person)
    }

    /// M6-4: whether the goal is done, after a run that finished the way it
    /// was asked to.
    ///
    /// One typed question, run on its own current-thread runtime because
    /// the settle path is synchronous. That is affordable here and would
    /// not be on the main model: this is a separate service, billed on its
    /// own, answering one question — the send path and the usage
    /// accounting that deferred this work are not involved.
    ///
    /// `None` whenever there is no judge, no answer, or an answer that is
    /// not confident. The caller leaves the run alone on `None`.
    fn judge_goal(&self, goal: &str) -> Option<bool> {
        let factory = self.trusted_runs.as_ref()?.factory.clone();
        let decider = factory.decider()?;
        // The tasks and what each one was accepted on. This is the state
        // the owner already keeps as the record of what was done, so the
        // question is asked against the same evidence a person would read
        // — not against the transcript, which is long and mostly working.
        let saved = self.store.snapshot().ok()?;
        let evidence = saved
            .state
            .tasks
            .iter()
            .map(|task| {
                let met = task
                    .acceptance
                    .iter()
                    .filter(|held| {
                        held.state == polaris_desktop_protocol::snapshot::AcceptanceState::Passed
                    })
                    .count();
                format!(
                    "- {} [{:?}] 受入 {}/{}",
                    task.title,
                    task.state,
                    met,
                    task.acceptance.len()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?
            .block_on(polaris_core::resume::achieved::judge(
                decider.as_ref(),
                goal,
                &evidence,
                std::future::pending(),
            ))
    }

    /// M6-2b: one automatic continuation, only while a goal is unfinished and
    /// nothing is unresolved. The budget is spent before the run is accepted,
    /// so a crash in between refuses instead of repeating.
    fn continue_goal(
        &mut self,
        out: Option<&Outbox>,
        stopped_by_person: bool,
    ) -> Result<(), ServiceError> {
        use polaris_core::resume::{Decision, Observed, decide};
        let saved = self.store.snapshot()?;
        let (Some(goal), Some(client)) = (saved.state.goal.clone(), self.client.clone()) else {
            return Ok(());
        };
        // The person's own text is never replaced by the goal, and their stop
        // is never re-read from a state that lost it.
        if stopped_by_person
            || !saved.state.draft.text.is_empty()
            || !saved.state.draft.attachment_ids.is_empty()
        {
            return Ok(());
        }
        let mut observed = Observed {
            goal_active: goal.active,
            goal_stopped: goal.stopped,
            previous: saved.state.runs.last().map(|record| record.run.state),
            awaiting_approval: !saved.state.unresolved_approvals.is_empty(),
            automatic_used: goal.automatic_used.get(),
            // Asked for below, and only when it could change the outcome.
            goal_achieved: None,
        };
        if decide(&observed) != Decision::Continue {
            // M6-4: a run can finish exactly as asked and still be one step
            // of the goal. That is the only case worth a question, so the
            // judge is asked here and nowhere else — after everything that
            // refuses for another reason has already refused.
            let judged = matches!(
                decide(&observed),
                Decision::Refuse {
                    reason: polaris_core::resume::Refusal::Completed
                }
            )
            .then(|| self.judge_goal(&goal.text))
            .flatten();
            if judged != Some(false) {
                return Ok(());
            }
            observed.goal_achieved = judged;
            if decide(&observed) != Decision::Continue {
                return Ok(());
            }
        }
        self.store.record_automatic_resume()?;
        let draft_revision = saved.state.draft.draft_revision;
        let request = |engine: &mut Self, body| Request {
            protocol_version: Default::default(),
            client_id: client.clone(),
            request_id: RequestId::new(engine.id("auto")).expect("bounded identifier"),
            body,
        };
        let draft = request(
            self,
            RequestBody::DraftUpdate(
                self.session.clone(),
                polaris_desktop_protocol::request::DraftUpdate {
                    expected_draft_revision: draft_revision,
                    text: goal.text.clone(),
                    attachment_ids: Vec::new(),
                },
            ),
        );
        let start = request(
            self,
            RequestBody::RunStart(
                self.session.clone(),
                polaris_desktop_protocol::request::RunStart {
                    mcp_prompt: None,
                    expected_draft_revision: DecimalU64::new(draft_revision.get() + 1),
                    expected_configuration_revision: saved
                        .state
                        .configuration
                        .configuration_revision,
                    expected_policy_revision: saved.state.policy_revision,
                },
            ),
        );
        let mut events = Vec::new();
        for automatic in [draft, start] {
            if self.handle(&automatic, &mut events).is_err() {
                // A refused continuation stays refused; the spent budget is
                // not returned, so it cannot be retried in a loop.
                break;
            }
        }
        if let Some(out) = out {
            self.emit_all(events, out)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
