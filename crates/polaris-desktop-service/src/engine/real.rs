//! Explicit trusted production integration; no provider discovery or source apply.
use super::*;
use polaris_core::{
    AgentEvent,
    agent::AgentError,
    audit::AuditLog,
    desktop_events::{DesktopEventReceiver, EVENT_CAPACITY, EventEnvelope, EventFailure},
    desktop_run::{DesktopRun, DesktopRunError, DesktopRunInput},
    desktop_store::{HistoryGap, SavedWorkflow},
    isolated_run::PreparedWorkspace,
    session::Session,
};
use polaris_provider::{Message, Provider, Role};

/// Trusted owner only. Called after the accepted input and run intent are durable.
/// Runs on the single owned prepare thread, never the reactor or Writer owner.
/// Must not start providers/tools. Own cleanup of partial preparation on error
/// or panic; returning an error does not establish that preparation had no effects.
/// The snapshot is the durable authority, including the accepted user message.
/// Skills and agent types are immutable trusted snapshots; no host disk reload.
/// Legacy history hooks remain unavailable.
pub trait TrustedRunFactory: Send + Sync {
    /// Immutable trusted catalog used to validate next-run role choices.
    fn role_catalog(&self) -> &[polaris_skills::AgentType] {
        &[]
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
        store: Writer,
        runs: Arc<dyn TrustedRunFactory>,
        source: SourceApplyRuntime,
    ) -> Result<Self, ServiceError> {
        let saved = store.snapshot()?;
        if store.coordinates() != (&saved.marker.project_id, &saved.marker.session_id) {
            return Err(ServiceError::Options);
        }
        let owner = source_owner::SourceOwner::new(source)?;
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
        }
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
                self.suffix = completion
                    .session
                    .messages
                    .into_iter()
                    .skip(self.prefix.len())
                    .collect();
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
        if history.is_some()
            != (expected == polaris_desktop_protocol::snapshot::HistoryMode::Strict10)
        {
            return Err(ServiceError::Options);
        }
        let Some(history) = history else {
            return Ok(None);
        };
        store.bind_memory(target, history.identity.clone())?;
        let summary = Arc::new(polaris_core::strict_provider::ProviderSummary::new(
            Arc::new(total.wrap(history.summary_provider)),
        ));
        self.summary_usage = Some(summary.usage.clone());
        self.embedding_usage = Some(history.embedding_usage);
        let strict = Arc::new(polaris_core::conversation_memory::StrictHistory::new(
            summary,
            history.embedder,
        ));
        let (memory, owner) = polaris_core::desktop_memory::DesktopMemory::pair(
            target.clone(),
            strict,
            history.database,
            history.identity,
        )
        .map_err(|_| ServiceError::Options)?;
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
        AgentEvent::SpawnStarted { agent_type, task } => {
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
                    state: RunState::Running,
                    task_ids: parent_tasks.clone(),
                },
                agent_type,
                task,
            )?
        }
        AgentEvent::SpawnFinished { agent_type, ok } => {
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
                real.prefix = published.raw.iter().map(|r| r.message.clone()).collect();
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
        let active = self.active.as_mut().ok_or(ServiceError::Worker)?;
        let real = active.real.as_mut().ok_or(ServiceError::Worker)?;
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
                        .map(|saved| saved.restore(None))
                        .transpose()?;
                    if self.disconnected.load(Ordering::Acquire) {
                        real.cancelled = true;
                        real.outcome = Some(Observation::Cancelled);
                        active.execution.cancel();
                    } else {
                        let mut inputs = real
                            .prepared_inputs
                            .take()
                            .expect("validated retained inputs");
                        let desktop_memory = real.prepare_memory(
                            inputs.history.take(),
                            &active.target,
                            &mut self.store,
                            &active.usage,
                        );
                        if desktop_memory.is_err() {
                            real.outcome = Some(Observation::Failed);
                        }
                        let input = DesktopRunInput {
                            run_id: active.target.run_id.clone(),
                            prepared: inputs.prepared,
                            execution: active.execution.port.clone(),
                            provider: Arc::new(
                                active
                                    .usage
                                    .wrap(Arc::new(real.main_usage.wrap(inputs.provider))),
                            ),
                            provider_pool: Arc::new(
                                active
                                    .usage
                                    .wrap(Arc::new(real.main_usage.wrap(inputs.provider_pool))),
                            ),
                            session: Session {
                                messages: real.prefix.clone(),
                                tool_memory: inputs.tool_memory,
                                workflow,
                                desktop_memory: desktop_memory.unwrap_or(None),
                                ..Session::new()
                            },
                            always_on: inputs.always_on,
                            audit: inputs.audit,
                            max_turns: inputs.max_turns,
                            skills: inputs.skills,
                            agent_types: inputs.agent_types,
                            spawn_concurrency: inputs.spawn_concurrency,
                            spawn_write_concurrency: inputs.spawn_write_concurrency,
                        };
                        if real.outcome.is_none() {
                            match DesktopRun::start(input) {
                                Ok((worker, events)) => {
                                    real.worker = Some(worker);
                                    real.events = Some(events);
                                }
                                Err(_) => real.outcome = Some(Observation::Failed),
                            }
                        }
                    }
                } else {
                    real.outcome = Some(Observation::Failed);
                }
            }
        }
        if real.outcome.is_some() {
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
            if real.output_rejected {
                self.store
                    .record_history_gap(&active.target, HistoryGap::OutputRejected)
                    .map_err(|_| StoreError::RecoveryRequired)?;
            }
            self.store.finish_with_messages_and_workflow(
                &active.target,
                &active.operation,
                outcome,
                result,
                real.suffix.clone(),
                real.workflow.clone(),
            )?;
        } else if !state.run.state.is_terminal() {
            self.store
                .finish(&active.target, Observation::Cancelled, result)?;
        }
        active.execution.saved = true;
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
        Ok(())
    }
}

#[cfg(test)]
mod tests;
