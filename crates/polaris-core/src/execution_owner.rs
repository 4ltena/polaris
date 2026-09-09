//! 共通のrun専用有界実行所有者。保存スレッドではcommandを実行しない。
#[cfg(target_os = "macos")]
use crate::isolated_run::PreparedWorkspace;
use crate::{
    desktop_execution::*,
    desktop_store::{IntentReceipt, StoreError, Writer},
};
use polaris_desktop_protocol::run_state::Observation;
use polaris_desktop_protocol::{
    event::{ApprovalExpired, EventBody, ExpiryReason},
    ids::{ApprovalId, DecimalU64, OperationId, ResultId},
    request::RunTarget,
    snapshot::{ApprovalDecision, ApprovalDisplay, PendingApproval, PendingApprovalState},
};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
fn prepared_policy_matches(workspace: &PreparedWorkspace, policy: &SandboxPolicy) -> bool {
    let Ok(narrowed) = workspace
        .policy()
        .restrict(policy.mode(), policy.writable_roots())
    else {
        return false;
    };
    let Some(actual) = policy.isolated_boundary() else {
        return false;
    };
    let expected = narrowed.isolated_boundary().unwrap();
    // restrict preserves reads/cwd/scratch; it only narrows source writes.
    actual.workspace == expected.workspace
        && actual.readable_roots == expected.readable_roots
        && actual.environment.as_ref().map(|e| (&e.home, &e.tmpdir))
            == expected.environment.as_ref().map(|e| (&e.home, &e.tmpdir))
        && policy.writable_roots() == narrowed.writable_roots()
        && policy
            .restrict(policy.mode(), policy.writable_roots())
            .is_ok()
}

const QUANTUM: usize = 4;
const APPROVAL_TTL_MS: u64 = 60_000;
const OPERATION_KIND: &str = "process.execute";
struct AwaitingExecution {
    request: ExecutionRequest,
    approval: PendingApproval,
    decision: Option<ApprovalDecision>,
    reservation: Reservation,
    deadline: StartDeadline,
}
struct StartDeadline {
    unix_ms: u64,
    monotonic: std::time::Instant,
}
impl StartDeadline {
    fn valid(&self) -> bool {
        std::time::Instant::now() < self.monotonic && now_ms().is_ok_and(|now| now < self.unix_ms)
    }
}
pub fn now_ms() -> Result<u64, StoreError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StoreError::RunConflict)?
        .as_millis();
    u64::try_from(now).map_err(|_| StoreError::Overflow)
}
/// Keep representable legacy IDs so existing intent/replay coordinates remain
/// unchanged. Long targets use an unambiguous tuple and a bounded SHA256 suffix.
fn execution_operation_id(target: &RunTarget, counter: u64) -> Result<OperationId, StoreError> {
    let legacy = format!("exec-{}-{counter}", target.run_id.as_str());
    if let Ok(operation) = OperationId::new(legacy.clone())
        && ApprovalId::new(format!("approval-{legacy}")).is_ok()
    {
        return Ok(operation);
    }
    let coordinates = serde_json::to_vec(&(
        "execution-operation-v2",
        target.run_id.as_str(),
        target.attempt_id.as_str(),
        counter,
    ))
    .map_err(|_| StoreError::Overflow)?;
    OperationId::new(format!(
        "exec-v2-{}",
        crate::conversation_state::content_hash(&coordinates)
    ))
    .map_err(|_| StoreError::Overflow)
}

type Completion = (Arc<ExecutionResult>, Option<PendingCleanup>);
// 保存ownerとは独立した単一回収主体。起動前からslotを共有所有する。
static RECOVERY: Mutex<Recovery> = Mutex::new(Recovery {
    slots: Vec::new(),
    pending: Vec::new(),
    cursor: 0,
    pending_cursor: 0,
});
static RECLAIMER: OnceLock<std::io::Result<JoinHandle<()>>> = OnceLock::new();

fn ensure_reclaimer() -> Result<(), String> {
    RECLAIMER
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("desktop-reclaimer".into())
                .spawn(|| {
                    loop {
                        RECOVERY.lock().unwrap_or_else(|e| e.into_inner()).step();
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
static RESERVED: AtomicUsize = AtomicUsize::new(0);
struct Reservation;
impl Reservation {
    fn acquire() -> Option<Self> {
        RESERVED
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < CAPACITY).then_some(n + 1)
            })
            .ok()
            .map(|_| Self)
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        RESERVED.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Slot {
    #[cfg(target_os = "macos")]
    workspace: Option<Arc<PreparedWorkspace>>,
    _reservation: Reservation,
    target: RunTarget,
    operation: OperationId,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    completion: Arc<Mutex<Option<Completion>>>,
    result: Option<Arc<ExecutionResult>>,
    pending: Option<Cleanup>,
    recorded: bool,
    orphaned: bool,
}
enum Cleanup {
    Native(PendingCleanup),
    // A panic may park an unattributed native handle in the shared registry.
    #[cfg(target_os = "macos")]
    Registry,
    #[cfg(any(test, feature = "execution-test-support"))]
    Deferred(Arc<AtomicBool>),
}
impl Cleanup {
    fn confirmed(&mut self) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Self::Registry => false,
            Self::Native(pending) => matches!(pending.try_reap(), Ok(Some(_))),
            #[cfg(any(test, feature = "execution-test-support"))]
            Self::Deferred(ready) => ready.load(Ordering::Acquire),
        }
    }
}
pub struct RunExecution {
    #[cfg(target_os = "macos")]
    workspace: Option<Arc<PreparedWorkspace>>,
    pub port: ExecutionPort,
    incoming: mpsc::Receiver<ExecutionRequest>,
    slots: Vec<Arc<Mutex<Slot>>>,
    awaiting: Vec<AwaitingExecution>,
    approval_events: Vec<EventBody>,
    next_operation: u64,
    cursor: usize,
    closed: bool,
    disconnected: Arc<AtomicBool>,
    pub saved: bool,
    #[cfg(any(test, feature = "execution-test-support"))]
    pub panic_worker: bool,
    #[cfg(any(test, feature = "execution-test-support"))]
    pub invalidate_after_intent: Option<bool>,
}
impl RunExecution {
    pub fn new(target: &RunTarget, disconnected: Arc<AtomicBool>) -> Self {
        let (port, incoming) = ExecutionPort::channel(target.run_id.clone());
        Self {
            #[cfg(target_os = "macos")]
            workspace: None,
            port,
            incoming,
            slots: Vec::new(),
            awaiting: Vec::new(),
            approval_events: Vec::new(),
            next_operation: 0,
            cursor: 0,
            closed: false,
            disconnected,
            saved: false,
            #[cfg(any(test, feature = "execution-test-support"))]
            panic_worker: false,
            #[cfg(any(test, feature = "execution-test-support"))]
            invalidate_after_intent: None,
        }
    }
    /// Trusted parent only: owns the sanitized copy and its read-helper grant.
    /// The parent retains its Arc for change collection/apply after execution.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)] // Production run_start is wired by the parent later.
    pub fn with_prepared_workspace(
        target: &RunTarget,
        disconnected: Arc<AtomicBool>,
        workspace: Arc<PreparedWorkspace>,
    ) -> Result<Self, String> {
        Self::with_prepared_workspace_and_mutations(target, disconnected, workspace, None)
    }
    /// Confirmed copy-write scope only; this grants no source apply authority.
    /// None retains the existing interactive mutation approval behavior.
    #[cfg(target_os = "macos")]
    pub fn with_prepared_workspace_and_mutations(
        target: &RunTarget,
        disconnected: Arc<AtomicBool>,
        workspace: Arc<PreparedWorkspace>,
        mutation_scope: Option<SandboxPolicy>,
    ) -> Result<Self, String> {
        Self::with_prepared_workspace_and_capabilities(
            target,
            disconnected,
            workspace,
            mutation_scope,
            None,
        )
    }
    /// Explicit confirmed folder execution ceiling; None preserves legacy behavior.
    #[cfg(target_os = "macos")]
    pub fn with_prepared_workspace_and_capabilities(
        target: &RunTarget,
        disconnected: Arc<AtomicBool>,
        workspace: Arc<PreparedWorkspace>,
        mutation_scope: Option<SandboxPolicy>,
        execution_capability: Option<ConfirmedExecutionCapability>,
    ) -> Result<Self, String> {
        if let Some(ConfirmedExecutionCapability::ConfinedCode { scope }) = &execution_capability {
            let normalized = scope
                .restrict(SandboxMode::WorkspaceWrite, scope.writable_roots())
                .map_err(|_| "invalid confirmed execution scope")?;
            let expected = workspace
                .policy()
                .restrict(SandboxMode::WorkspaceWrite, scope.writable_roots())
                .map_err(|_| "execution scope exceeds prepared workspace")?;
            if normalized != expected {
                return Err("execution scope differs from prepared policy".into());
            }
        }
        if let Some(scope) = &mutation_scope {
            let expected = workspace
                .policy()
                .restrict(scope.mode(), scope.writable_roots())
                .map_err(|_| "mutation scope exceeds prepared workspace")?;
            if expected != *scope {
                return Err("mutation scope differs from prepared policy".into());
            }
        }
        let readonly = workspace
            .policy()
            .restrict(SandboxMode::ReadOnly, &[])
            .map_err(|_| "invalid prepared execution boundary")?;
        let (mut port, incoming) = ExecutionPort::channel_with_read_helper_and_mutations(
            target.run_id.clone(),
            workspace.helper_path().into(),
            readonly,
            mutation_scope,
        )?;
        port.configure_execution_capability(execution_capability)?;
        let mut owner = Self::new(target, disconnected);
        owner.port = port;
        owner.incoming = incoming;
        owner.workspace = Some(workspace);
        Ok(owner)
    }
    fn policy_matches(&self, policy: &SandboxPolicy) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(workspace) = &self.workspace {
            return prepared_policy_matches(workspace, policy);
        }
        let _ = policy;
        true // Existing fake constructor has no prepared-workspace contract.
    }
    pub fn cancel(&mut self) {
        self.closed = true;
        self.incoming.close();
        for slot in &self.slots {
            slot.lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancelled
                .store(true, Ordering::Release);
        }
    }
    /// 保存済intent→slot→workerの順を、この保存owner上で固定する。
    pub fn step(&mut self, store: &mut Writer, target: &RunTarget) -> Result<(), StoreError> {
        debug_assert_eq!(self.port.run_id(), &target.run_id);
        let now = now_ms()?;
        let published = store.snapshot()?;
        let policy_revision = published.state.policy_revision;
        let run_policy_matches = published.state.runs.iter().any(|run| {
            run.run.run_id == target.run_id
                && run.run.attempt_id == target.attempt_id
                && run.policy_revision == policy_revision
                && !run.run.state.is_terminal()
        });
        let mut cursor = 0;
        while cursor < self.awaiting.len() {
            let waiting = &self.awaiting[cursor];
            let cancelled = self.closed
                || self.disconnected.load(Ordering::Acquire)
                || waiting.request.cancelled.load(Ordering::Acquire);
            let revoked = !run_policy_matches
                || waiting.approval.policy_revision != policy_revision
                || !self.policy_matches(&waiting.request.command.policy);
            let expired = !waiting.deadline.valid();
            if cancelled || revoked || expired {
                store.invalidate_approval(&waiting.approval.approval_id)?;
                let waiting = self.awaiting.remove(cursor);
                self.approval_events
                    .push(EventBody::ApprovalExpired(ApprovalExpired {
                        approval_id: waiting.approval.approval_id,
                        reason: if cancelled {
                            ExpiryReason::Cancelled
                        } else if revoked {
                            ExpiryReason::PolicyRevoked
                        } else {
                            ExpiryReason::Expired
                        },
                    }));
                let _ = waiting
                    .request
                    .reply
                    .send(Arc::new(ExecutionResult::not_started(
                        "execution approval is no longer valid".into(),
                    )));
                continue;
            }
            let Some(decision) = waiting.decision else {
                cursor += 1;
                continue;
            };
            if decision == ApprovalDecision::Deny {
                let waiting = self.awaiting.remove(cursor);
                let _ = waiting
                    .request
                    .reply
                    .send(Arc::new(ExecutionResult::not_started(
                        "execution approval denied".into(),
                    )));
                continue;
            }
            // 一回限りの許可消費とintent保存を終えるまで、workerを作らない。
            let hash = waiting
                .request
                .command
                .binding_hash()
                .map_err(|_| StoreError::RunConflict)?;
            let receipt = store.consume_approval_intent(
                target,
                &waiting.approval.approval_id,
                &waiting.approval.operation_id,
                OPERATION_KIND,
                &waiting.request.command.policy.describe(),
                &hash,
                now_ms()?,
            )?;
            if receipt != IntentReceipt::NewlyPublished {
                return Err(StoreError::RunConflict);
            }
            let waiting = self.awaiting.remove(cursor);
            self.start_request(
                waiting.request,
                waiting.reservation,
                target,
                waiting.approval.operation_id,
                waiting.deadline,
                false,
            );
        }
        for _ in 0..QUANTUM {
            let Ok(request) = self.incoming.try_recv() else {
                break;
            };
            if self.closed
                || self.disconnected.load(Ordering::Acquire)
                || !run_policy_matches
                || request.cancelled.load(Ordering::Acquire)
                || self.slots.len() + self.awaiting.len() >= CAPACITY
            {
                let _ = request.reply.send(Arc::new(ExecutionResult::failed(
                    "execution rejected before approval".into(),
                )));
                continue;
            }
            // Classify once: a later grant expiry must reject at start, never
            // reclassify a stamped mutation as an ordinary approval request.
            let automatic = request.is_authorized_automatic_for(&target.run_id);
            if ((request.has_mutation_authorization() || request.has_execution_authorization())
                && !automatic)
                || !self.port.permits_request(&request, &target.run_id)
            {
                let _ = request.reply.send(Arc::new(ExecutionResult::not_started(
                    "folder execution authorization is no longer valid".into(),
                )));
                continue;
            }
            if !self.policy_matches(&request.command.policy) {
                let _ = request.reply.send(Arc::new(ExecutionResult::not_started(
                    "execution policy escapes prepared workspace".into(),
                )));
                continue;
            }
            if let Err(error) = ensure_reclaimer() {
                let _ = request.reply.send(Arc::new(ExecutionResult::failed(error)));
                continue;
            }
            let Some(reservation) = Reservation::acquire() else {
                let _ = request.reply.send(Arc::new(ExecutionResult::failed(
                    "service execution capacity exhausted".into(),
                )));
                continue;
            };
            self.next_operation = self
                .next_operation
                .checked_add(1)
                .ok_or(StoreError::Overflow)?;
            let operation = execution_operation_id(target, self.next_operation)?;
            if automatic {
                if store.record_intent(target, operation.clone())? != IntentReceipt::NewlyPublished
                {
                    return Err(StoreError::RunConflict);
                }
                let deadline = StartDeadline {
                    unix_ms: now.checked_add(5_000).ok_or(StoreError::Overflow)?,
                    monotonic: std::time::Instant::now() + std::time::Duration::from_secs(5),
                };
                self.start_request(request, reservation, target, operation, deadline, true);
                continue;
            }
            let approval = PendingApproval {
                approval_id: ApprovalId::new(format!("approval-{}", operation.as_str()))
                    .map_err(|_| StoreError::Overflow)?,
                run_id: target.run_id.clone(),
                attempt_id: target.attempt_id.clone(),
                operation_id: operation,
                operation: OPERATION_KIND.into(),
                scope: request.command.policy.describe(),
                payload_hash: request
                    .command
                    .binding_hash()
                    .map_err(|_| StoreError::RunConflict)?,
                policy_revision,
                expires_at_unix_ms: DecimalU64::new(
                    now.checked_add(APPROVAL_TTL_MS)
                        .ok_or(StoreError::Overflow)?,
                ),
                state: PendingApprovalState::Pending,
                display: ApprovalDisplay {
                    title: "コマンド実行".into(),
                    description: format!(
                        "{}\n{}",
                        excerpt(
                            &format!(
                                "{} {:?}",
                                request.command.program.display(),
                                request.command.args
                            ),
                            1000
                        ),
                        excerpt(
                            &request.command.input_description().unwrap_or_default(),
                            1000
                        ),
                    ),
                    choices: vec![ApprovalDecision::Allow, ApprovalDecision::Deny],
                },
            };
            let deadline = StartDeadline {
                unix_ms: approval.expires_at_unix_ms.get(),
                monotonic: std::time::Instant::now()
                    + std::time::Duration::from_millis(APPROVAL_TTL_MS),
            };
            store.publish_approval(approval.clone(), now_ms()?)?;
            self.approval_events
                .push(EventBody::ApprovalRequested(approval.clone()));
            self.awaiting.push(AwaitingExecution {
                request,
                approval,
                decision: None,
                reservation,
                deadline,
            });
        }
        for _ in 0..QUANTUM.min(self.slots.len()) {
            self.cursor %= self.slots.len();
            let owned = &self.slots[self.cursor];
            self.cursor += 1;
            let operation = {
                let mut slot = owned.lock().unwrap_or_else(|e| e.into_inner());
                debug_assert_eq!(&slot.target, target);
                slot.poll();
                (slot.worker.is_none()
                    && slot.pending.is_none()
                    && slot.result.is_some()
                    && !slot.recorded)
                    .then(|| slot.operation.clone())
            };
            if let Some(operation) = operation {
                // fsync中にも独立回収スレッドがslotを取得できるようにする。
                store.record_operation_result(
                    target,
                    &operation,
                    ResultId::new(format!("result-{}", operation.as_str()))
                        .map_err(|_| StoreError::Overflow)?,
                )?;
                owned.lock().unwrap_or_else(|e| e.into_inner()).recorded = true;
            }
        }
        Ok(())
    }
    fn start_request(
        &mut self,
        request: ExecutionRequest,
        reservation: Reservation,
        target: &RunTarget,
        operation: OperationId,
        deadline: StartDeadline,
        trusted_automatic: bool,
    ) {
        #[cfg(any(test, feature = "execution-test-support"))]
        let deadline = {
            let mut deadline = deadline;
            if let Some(disconnect) = self.invalidate_after_intent.take() {
                if disconnect {
                    self.disconnected.store(true, Ordering::Release);
                } else {
                    deadline.monotonic = std::time::Instant::now();
                }
            }
            deadline
        };
        let completion = Arc::new(Mutex::new(None));
        let owned = Arc::new(Mutex::new(Slot {
            #[cfg(target_os = "macos")]
            workspace: self.workspace.clone(),
            _reservation: reservation,
            target: target.clone(),
            operation,
            cancelled: request.cancelled.clone(),
            worker: None,
            completion: completion.clone(),
            result: None,
            pending: None,
            recorded: false,
            orphaned: false,
        }));
        RECOVERY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .push(owned.clone());
        self.slots.push(owned.clone());
        let mut slot = owned.lock().unwrap_or_else(|e| e.into_inner());
        let run_id = target.run_id.clone();
        let cancelled = slot.cancelled.clone();
        let disconnected = self.disconnected.clone();
        let execution_port = self.port.clone();
        if cancelled.load(Ordering::Acquire)
            || disconnected.load(Ordering::Acquire)
            || !deadline.valid()
            || !self.port.permits_request(&request, &run_id)
            || !self.policy_matches(&request.command.policy)
            || (trusted_automatic && !request.is_authorized_automatic_for(&run_id))
        {
            let result = Arc::new(ExecutionResult::not_started(
                "execution start authorization expired or cancelled".into(),
            ));
            slot.result = Some(result.clone());
            let _ = request.reply.send(result);
            return;
        }
        #[cfg(any(test, feature = "execution-test-support"))]
        let panic_worker = self.panic_worker;
        #[cfg(target_os = "macos")]
        let workspace = self.workspace.clone();
        let worker = std::thread::Builder::new()
            .name("desktop-execution".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(any(test, feature = "execution-test-support"))]
                    assert!(!panic_worker, "injected worker panic");
                    run_confined_controlled_authorized(
                        &request.command.policy,
                        &request.command.program,
                        &request.command.args,
                        request.command.stdin.as_deref(),
                        || {
                            cancelled.load(Ordering::Acquire)
                                || disconnected.load(Ordering::Acquire)
                        },
                        || {
                            #[cfg(target_os = "macos")]
                            if workspace.as_ref().is_some_and(|workspace| {
                                !prepared_policy_matches(workspace, &request.command.policy)
                            }) {
                                return false;
                            }
                            execution_port.permits_request(&request, &run_id)
                                && deadline.valid()
                                && (!trusted_automatic
                                    || request.is_authorized_automatic_for(&run_id))
                        },
                    )
                }));
                let (result, pending) = match outcome {
                    Ok(Ok(outcome)) => ExecutionResult::from_outcome(outcome),
                    Ok(Err(error)) => (ExecutionResult::failed(error.to_string()), None),
                    Err(_) => (
                        ExecutionResult::failed(
                            "execution worker panicked; service must inspect cleanup registry"
                                .into(),
                        ),
                        None,
                    ),
                };
                let result = Arc::new(result);
                *completion.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((result.clone(), pending));
                // 通知先が消えていても、結果とcleanupはslot内に既にある。
                let _ = request.reply.send(result);
                #[cfg(target_os = "macos")]
                drop(workspace);
            });
        match worker {
            Ok(worker) => slot.worker = Some(worker),
            Err(error) => {
                slot.result = Some(Arc::new(ExecutionResult::not_started(error.to_string())))
            }
        }
    }
    pub fn approval_resolved(&mut self, id: &ApprovalId, decision: ApprovalDecision) {
        if let Some(waiting) = self
            .awaiting
            .iter_mut()
            .find(|w| &w.approval.approval_id == id)
        {
            waiting.decision = Some(decision);
        }
    }
    pub fn take_approval_events(&mut self) -> Vec<EventBody> {
        std::mem::take(&mut self.approval_events)
    }
    pub fn awaiting_approval(&self) -> bool {
        !self.awaiting.is_empty()
    }
    pub fn quiescent(&self) -> bool {
        self.incoming.is_empty()
            && self.awaiting.is_empty()
            && self.slots.iter().all(|owned| {
                let s = owned.lock().unwrap_or_else(|e| e.into_inner());
                s.worker.is_none() && s.pending.is_none() && s.recorded
            })
    }
    pub fn observation(&self, requested: Observation) -> Observation {
        if requested != Observation::Succeeded {
            return requested;
        }
        if self
            .slots
            .iter()
            .filter_map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).result.clone())
            .any(|r| r.problem.is_some() || r.status != Some(0) || r.end != ControlledEnd::Exited)
        {
            Observation::Failed
        } else {
            requested
        }
    }
    #[cfg(any(test, feature = "execution-test-support"))]
    pub fn cleanup_probe_for_test(&self) -> Box<dyn Fn() -> bool + Send> {
        let slots = self.slots.clone();
        Box::new(move || {
            !slots.is_empty()
                && slots.iter().all(|owned| {
                    let slot = owned.lock().unwrap_or_else(|e| e.into_inner());
                    slot.worker.is_none() && slot.pending.is_none() && slot.result.is_some()
                })
        })
    }
    #[cfg(any(test, feature = "execution-test-support"))]
    pub fn cleanup_complete_for_test(&self) -> bool {
        !self.slots.is_empty()
            && self.slots.iter().all(|owned| {
                let s = owned.lock().unwrap_or_else(|e| e.into_inner());
                s.worker.is_none() && s.pending.is_none() && s.result.is_some()
            })
    }
    #[cfg(any(test, feature = "execution-test-support"))]
    pub fn inject_unconfirmed_cleanup(&mut self) -> Arc<AtomicBool> {
        let ready = Arc::new(AtomicBool::new(false));
        let mut slot = self
            .slots
            .first()
            .unwrap()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        slot.poll();
        assert!(slot.worker.is_none() && slot.pending.is_none());
        slot.pending = Some(Cleanup::Deferred(ready.clone()));
        ready
    }
    pub fn append_results(&self, text: &mut String) {
        if self.slots.is_empty() {
            return;
        }
        const LIMIT: usize = 32 * 1024;
        let original_budget = if self.slots.len() > 16 {
            1024
        } else {
            LIMIT / 2
        };
        let original = excerpt(text, original_budget);
        *text = original;
        let budget = (LIMIT - text.len()) / self.slots.len();
        for owned in &self.slots {
            let slot = owned.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(result) = &slot.result {
                let header = format!(
                    "\n[operation={} execution end={:?} status={:?} stdout_truncated={} stderr_truncated={} stdout_eof={} stderr_eof={} problem={}]\n",
                    slot.operation.as_str(),
                    result.end,
                    result.status,
                    result.stdout_truncated,
                    result.stderr_truncated,
                    result.stdout_eof,
                    result.stderr_eof,
                    excerpt(result.problem.as_deref().unwrap_or("none"), 192)
                );
                let remaining = budget.saturating_sub(header.len());
                text.push_str(&header);
                text.push_str(&excerpt(&result.stdout, remaining / 2));
                text.push_str(&excerpt(&result.stderr, remaining - remaining / 2));
            }
        }
        debug_assert!(text.len() <= LIMIT);
    }
}
impl Drop for RunExecution {
    fn drop(&mut self) {
        self.cancel();
        let mut recovery = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
        for owned in &self.slots {
            let mut slot = owned.lock().unwrap_or_else(|e| e.into_inner());
            slot.poll();
            if self.saved && slot.worker.is_none() && slot.pending.is_none() {
                recovery.slots.retain(|slot| !Arc::ptr_eq(slot, owned));
            } else {
                slot.orphaned = true;
            }
        }
    }
}
impl Slot {
    fn poll(&mut self) {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let joined = self.worker.take().unwrap().join();
            if let Some((result, pending)) = self
                .completion
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                self.result = Some(result);
                self.pending = pending.map(Cleanup::Native);
                #[cfg(target_os = "macos")]
                if self.workspace.is_some()
                    && self.pending.is_none()
                    && self
                        .result
                        .as_ref()
                        .is_some_and(|r| r.end == ControlledEnd::StopUnconfirmed)
                {
                    self.pending = Some(Cleanup::Registry);
                }
            } else {
                #[cfg(target_os = "macos")]
                if self.workspace.is_some() {
                    self.pending = Some(Cleanup::Registry);
                }
                self.result = Some(Arc::new(ExecutionResult::failed(
                    if joined.is_err() {
                        "execution worker panicked"
                    } else {
                        "execution worker lost result"
                    }
                    .into(),
                )));
            }
        }
        if self.pending.as_mut().is_some_and(Cleanup::confirmed) {
            self.pending = None;
        }
        #[cfg(target_os = "macos")]
        if self.worker.is_none() && self.pending.is_none() && self.result.is_some() {
            self.workspace = None;
        }
    }
}

/// sandbox退避registryを読む場所はserviceのここだけ。PIDでrunへ帰属させない。
struct Recovery {
    slots: Vec<Arc<Mutex<Slot>>>,
    pending: Vec<Cleanup>,
    cursor: usize,
    pending_cursor: usize,
}
impl Recovery {
    fn step(&mut self) {
        self.step_with_registry(|| {
            take_pending_cleanups()
                .into_iter()
                .map(Cleanup::Native)
                .collect()
        });
    }
    // Private drain seam also lets synthetic tests fix the late-publication
    // ordering without launching a native process or inferring ownership by PID.
    fn step_with_registry(&mut self, mut drain: impl FnMut() -> Vec<Cleanup>) {
        for _ in 0..QUANTUM.min(self.slots.len()) {
            self.cursor %= self.slots.len();
            self.slots[self.cursor]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .poll();
            self.cursor += 1;
        }
        self.pending.extend(drain());
        for _ in 0..QUANTUM.min(self.pending.len()) {
            self.pending_cursor %= self.pending.len();
            if self.pending[self.pending_cursor].confirmed() {
                drop(self.pending.swap_remove(self.pending_cursor));
            } else {
                self.pending_cursor += 1;
            }
        }
        // Registration is serialized by RECOVERY, but service-side Slot::poll
        // can join concurrently with the first drain. Observe all joins BEFORE
        // the final drain: a worker may have parked a handle after the first one.
        #[cfg(target_os = "macos")]
        if self.pending.is_empty()
            && self.slots.iter().all(|owned| {
                let slot = owned.lock().unwrap_or_else(|e| e.into_inner());
                slot.worker.is_none() && slot.result.is_some()
            })
        {
            self.pending.extend(drain());
            if !self.pending.is_empty() {
                // Reap in a later bounded step. Registry leases remain held
                // until every unattributed handle is actually confirmed gone.
                return;
            }
            for owned in &self.slots {
                let mut slot = owned.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(slot.pending, Some(Cleanup::Registry)) {
                    slot.pending = None;
                    slot.poll();
                }
            }
        }
    }
    fn is_empty(&self) -> bool {
        self.pending.is_empty()
            && !self
                .slots
                .iter()
                .any(|s| s.lock().unwrap_or_else(|e| e.into_inner()).orphaned)
    }
}
#[derive(Default)]
pub struct CleanupOwner {
    _private: (),
}
impl CleanupOwner {
    pub fn step(&mut self) {
        RECOVERY.lock().unwrap_or_else(|e| e.into_inner()).step();
    }
    pub fn is_empty(&self) -> bool {
        RECOVERY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    const MARKER: &str = "\n[execution transcript truncated]\n";
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit.saturating_sub(MARKER.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut value = text[..end].to_owned();
    if limit >= MARKER.len() {
        value.push_str(MARKER);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn read_owner() -> (
        crate::desktop_store::PrototypeRoot,
        Writer,
        RunTarget,
        RunExecution,
        tempfile::TempDir,
    ) {
        read_owner_for_target(RunTarget {
            run_id: polaris_desktop_protocol::ids::RunId::new("read-run").unwrap(),
            attempt_id: polaris_desktop_protocol::ids::AttemptId::new("attempt").unwrap(),
        })
    }
    fn read_owner_for_target(
        target: RunTarget,
    ) -> (
        crate::desktop_store::PrototypeRoot,
        Writer,
        RunTarget,
        RunExecution,
        tempfile::TempDir,
    ) {
        use crate::desktop_store::{InitialState, PrototypeRoot};
        use polaris_desktop_protocol::{
            ids::*,
            request::*,
            snapshot::{Configuration, Draft},
        };
        use std::os::unix::fs::PermissionsExt;
        let root = PrototypeRoot::new().unwrap();
        let session = SessionId::new("read-session").unwrap();
        let mut writer = root
            .create(
                ProjectId::new("project").unwrap(),
                session.clone(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: "dummy".into(),
                        attachment_ids: vec![],
                    },
                    configuration: Configuration {
                        configuration_revision: DecimalU64::new(0),
                        provider: "fake".into(),
                        model: "fake".into(),
                        effort: "medium".into(),
                    },
                    policy_revision: DecimalU64::new(0),
                },
            )
            .unwrap();
        writer
            .apply(
                &Request {
                    protocol_version: Default::default(),
                    client_id: ClientId::new("client").unwrap(),
                    request_id: RequestId::new("start").unwrap(),
                    body: RequestBody::RunStart(
                        session,
                        RunStart {
                            expected_draft_revision: DecimalU64::new(0),
                            expected_configuration_revision: DecimalU64::new(0),
                            expected_policy_revision: DecimalU64::new(0),
                        },
                    ),
                },
                Some(target.clone()),
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for name in ["workspace", "workspace/home", "workspace/tmp", "runtime"] {
            std::fs::create_dir(base.join(name)).unwrap();
        }
        let helper = base.join("runtime/helper");
        std::fs::write(&helper, "dummy, must never execute").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = SandboxPolicy::isolated(
            SandboxMode::ReadOnly,
            &base.join("workspace"),
            &[base.join("runtime")],
        )
        .unwrap()
        .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
        .unwrap();
        let (port, incoming) =
            ExecutionPort::channel_with_read_helper(target.run_id.clone(), helper, policy).unwrap();
        let mut owner = RunExecution::new(&target, Arc::new(AtomicBool::new(false)));
        owner.port = port;
        owner.incoming = incoming;
        (root, writer, target, owner, dir)
    }
    #[test]
    fn execution_ids_bound_long_utf8_targets_and_preserve_short_legacy_ids() {
        use polaris_desktop_protocol::ids::{AttemptId, RunId};
        for run in [
            "x".repeat(128),
            "界".repeat(42),
            format!("{}ab", "界".repeat(42)),
        ] {
            let target = RunTarget {
                run_id: RunId::new(run).unwrap(),
                attempt_id: AttemptId::new("試行".repeat(21)).unwrap(),
            };
            let first = execution_operation_id(&target, 1).unwrap();
            assert!(ApprovalId::new(format!("approval-{}", first.as_str())).is_ok());
            assert_eq!(first, execution_operation_id(&target, 1).unwrap());
            assert_ne!(first, execution_operation_id(&target, 2).unwrap());
            assert_ne!(
                first,
                execution_operation_id(
                    &RunTarget {
                        attempt_id: AttemptId::new("other").unwrap(),
                        ..target.clone()
                    },
                    1
                )
                .unwrap()
            );
            assert_ne!(
                first,
                execution_operation_id(
                    &RunTarget {
                        run_id: RunId::new("y".repeat(128)).unwrap(),
                        ..target.clone()
                    },
                    1
                )
                .unwrap()
            );
            let (_root, mut writer, target, mut owner, _dir) = read_owner_for_target(target);
            let waiting = owner
                .port
                .submit(ExecutionCommand {
                    policy: SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap(),
                    program: "/missing".into(),
                    args: vec![],
                    stdin: None,
                })
                .unwrap();
            owner.step(&mut writer, &target).unwrap();
            assert_eq!(owner.awaiting.len(), 1);
            assert_eq!(owner.awaiting[0].approval.operation_id, first);
            assert_eq!(
                writer.snapshot().unwrap().state.unresolved_approvals.len(),
                1
            );
            assert!(owner.slots.is_empty());
            drop(waiting);
        }
        let target = RunTarget {
            run_id: RunId::new("read-run").unwrap(),
            attempt_id: AttemptId::new("attempt").unwrap(),
        };
        assert_eq!(
            execution_operation_id(&target, 1).unwrap().as_str(),
            "exec-read-run-1"
        );
    }

    #[tokio::test]
    async fn invalid_mutation_stamp_never_publishes_approval_or_intent() {
        for change_runtime in [true, false] {
            let (_root, mut writer, target, mut owner, dir) = read_owner();
            let base = dir.path().canonicalize().unwrap();
            let selected = base.join("workspace/selected");
            std::fs::create_dir(&selected).unwrap();
            let scope = SandboxPolicy::isolated(
                SandboxMode::WorkspaceWrite,
                &base.join("workspace"),
                &[base.join("runtime")],
            )
            .unwrap()
            .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
            .unwrap();
            let child = scope
                .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&selected))
                .unwrap();
            let (port, mut incoming) = ExecutionPort::channel_with_read_helper_and_mutations(
                target.run_id.clone(),
                base.join("runtime/helper"),
                scope.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
                Some(scope.clone()),
            )
            .unwrap();
            let waiting = port
                .submit_confined_mutation(
                    &child,
                    &base.join("runtime/helper"),
                    &polaris_sandbox::Mutation::Write {
                        path: selected.join("new"),
                        content: "never installed".into(),
                    },
                )
                .unwrap();
            let mut request = incoming.try_recv().unwrap();
            assert!(request.is_authorized_mutation_for(&target.run_id));
            request.command.policy = if change_runtime {
                child
                    .with_isolated_runtime_bins(&[base.join("runtime")])
                    .unwrap()
            } else {
                scope
            };
            assert!(!request.is_authorized_mutation_for(&target.run_id));
            let (sender, incoming) = mpsc::channel(CAPACITY);
            owner.incoming = incoming;
            assert!(sender.try_send(request).is_ok());
            let before = writer.snapshot().unwrap();
            owner.step(&mut writer, &target).unwrap();
            assert_eq!(
                waiting.await.unwrap().end,
                ControlledEnd::CancelledBeforeSpawn
            );
            assert!(owner.awaiting.is_empty());
            assert!(owner.approval_events.is_empty());
            assert!(owner.slots.is_empty());
            assert_eq!(writer.snapshot().unwrap().marker, before.marker);
            assert!(
                writer.snapshot().unwrap().state.runs[0]
                    .operations
                    .is_empty()
            );
            assert!(!selected.join("new").exists());
        }
    }

    #[tokio::test]
    async fn fixed_ceiling_refuses_injected_generic_even_with_allow() {
        let (_root, mut writer, target, mut owner, _dir) = read_owner();
        owner
            .port
            .configure_execution_capability(Some(ConfirmedExecutionCapability::FixedHelpersOnly))
            .unwrap();
        let (plain, mut incoming) = ExecutionPort::channel(target.run_id.clone());
        let waiting = plain
            .submit(ExecutionCommand {
                policy: SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap(),
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "true".into()],
                stdin: None,
            })
            .unwrap();
        let (sender, receiver) = mpsc::channel(CAPACITY);
        owner.incoming = receiver;
        assert!(sender.try_send(incoming.try_recv().unwrap()).is_ok());
        owner.approval_resolved(
            &ApprovalId::new("approval-exec-read-run-1").unwrap(),
            ApprovalDecision::Allow,
        );
        owner.step(&mut writer, &target).unwrap();
        assert_eq!(
            waiting.await.unwrap().end,
            ControlledEnd::CancelledBeforeSpawn
        );
        assert!(owner.awaiting.is_empty());
        assert!(owner.approval_events.is_empty());
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
    }

    #[tokio::test]
    async fn invalid_execution_stamp_never_publishes_approval_or_intent() {
        for change_runtime in [true, false] {
            let (_root, mut writer, target, mut owner, dir) = read_owner();
            let base = dir.path().canonicalize().unwrap();
            let selected = base.join("workspace/selected");
            std::fs::create_dir(&selected).unwrap();
            let scope = SandboxPolicy::isolated(
                SandboxMode::WorkspaceWrite,
                &base.join("workspace"),
                &[base.join("runtime")],
            )
            .unwrap()
            .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
            .unwrap();
            let child = scope
                .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&selected))
                .unwrap();
            let (mut port, mut incoming) = ExecutionPort::channel_with_read_helper_and_mutations(
                target.run_id.clone(),
                base.join("runtime/helper"),
                scope.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
                Some(scope.clone()),
            )
            .unwrap();
            port.configure_execution_capability(Some(ConfirmedExecutionCapability::ConfinedCode {
                scope: scope.clone(),
            }))
            .unwrap();
            owner.port = port.clone();
            let waiting = port
                .submit_confined_execution(&child, &base.join("runtime/helper"), "touch new")
                .unwrap();
            let mut request = incoming.try_recv().unwrap();
            assert!(request.is_authorized_execution_for(&target.run_id));
            request.command.policy = if change_runtime {
                child
                    .with_isolated_runtime_bins(&[base.join("runtime")])
                    .unwrap()
            } else {
                scope
            };
            assert!(!request.is_authorized_execution_for(&target.run_id));
            let (sender, incoming) = mpsc::channel(CAPACITY);
            owner.incoming = incoming;
            assert!(sender.try_send(request).is_ok());
            let before = writer.snapshot().unwrap();
            owner.step(&mut writer, &target).unwrap();
            assert_eq!(
                waiting.await.unwrap().end,
                ControlledEnd::CancelledBeforeSpawn
            );
            assert!(owner.awaiting.is_empty());
            assert!(owner.approval_events.is_empty());
            assert!(owner.slots.is_empty());
            assert_eq!(writer.snapshot().unwrap().marker, before.marker);
            assert!(
                writer.snapshot().unwrap().state.runs[0]
                    .operations
                    .is_empty()
            );
            assert!(!selected.join("new").exists());
        }
    }

    #[tokio::test]
    async fn trusted_read_saves_intent_without_approval_and_rechecks_start() {
        for disconnect in [false, true] {
            let (_root, mut writer, target, mut owner, _dir) = read_owner();
            owner.invalidate_after_intent = Some(disconnect);
            let waiting = owner
                .port
                .submit_confined_read(&ConfinedReadRequest::DiffBefore {
                    path: "dummy".into(),
                })
                .unwrap();
            owner.step(&mut writer, &target).unwrap();
            assert!(owner.awaiting.is_empty());
            assert!(owner.approval_events.is_empty());
            assert_eq!(owner.slots.len(), 1);
            let result = waiting.await.unwrap();
            assert_eq!(result.end, ControlledEnd::CancelledBeforeSpawn);
            let saved = writer.snapshot().unwrap();
            assert_eq!(saved.state.runs[0].operations.len(), 1);
            assert!(saved.state.runs[0].operations[0].result_id.is_some());
            // Finish this fixture through the real publication boundary. Leaving
            // an unsaved owner here intentionally blocks the service-wide reaper,
            // contaminating unrelated shutdown tests in the same process.
            assert!(owner.quiescent());
            writer
                .finish(
                    &target,
                    Observation::Cancelled,
                    ResultId::new("trusted-read-cancelled").unwrap(),
                )
                .unwrap();
            owner.saved = true;
        }
    }
    #[tokio::test]
    async fn mutation_grant_saves_intent_without_approval_and_cancels_before_spawn() {
        for disconnect in [false, true] {
            let (_root, mut writer, target, mut owner, dir) = read_owner();
            let base = dir.path().canonicalize().unwrap();
            let scope = SandboxPolicy::isolated(
                SandboxMode::WorkspaceWrite,
                &base.join("workspace"),
                &[base.join("runtime")],
            )
            .unwrap()
            .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
            .unwrap();
            let (port, incoming) = ExecutionPort::channel_with_read_helper_and_mutations(
                target.run_id.clone(),
                base.join("runtime/helper"),
                scope.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
                Some(scope.clone()),
            )
            .unwrap();
            owner.port = port;
            owner.incoming = incoming;
            owner.invalidate_after_intent = Some(disconnect);
            let waiting = owner
                .port
                .submit_confined_mutation(
                    &scope,
                    &base.join("runtime/helper"),
                    &polaris_sandbox::Mutation::Write {
                        path: "new".into(),
                        content: "never installed".into(),
                    },
                )
                .unwrap();
            owner.step(&mut writer, &target).unwrap();
            assert!(owner.awaiting.is_empty());
            assert!(owner.approval_events.is_empty());
            assert_eq!(
                waiting.await.unwrap().end,
                ControlledEnd::CancelledBeforeSpawn
            );
            assert!(!base.join("workspace/new").exists());
            let saved = writer.snapshot().unwrap();
            assert_eq!(saved.state.runs[0].operations.len(), 1);
            assert!(saved.state.runs[0].operations[0].result_id.is_some());
            assert!(owner.quiescent());
            writer
                .finish(
                    &target,
                    Observation::Cancelled,
                    ResultId::new("mutation-cancelled").unwrap(),
                )
                .unwrap();
            owner.saved = true;
        }
    }

    #[tokio::test]
    async fn execution_grant_saves_intent_without_approval_and_cancels_before_spawn() {
        for disconnect in [false, true] {
            let (_root, mut writer, target, mut owner, dir) = read_owner();
            let base = dir.path().canonicalize().unwrap();
            let scope = SandboxPolicy::isolated(
                SandboxMode::WorkspaceWrite,
                &base.join("workspace"),
                &[base.join("runtime")],
            )
            .unwrap()
            .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
            .unwrap();
            let (mut port, incoming) = ExecutionPort::channel_with_read_helper_and_mutations(
                target.run_id.clone(),
                base.join("runtime/helper"),
                scope.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
                Some(scope.clone()),
            )
            .unwrap();
            port.configure_execution_capability(Some(ConfirmedExecutionCapability::ConfinedCode {
                scope: scope.clone(),
            }))
            .unwrap();
            owner.port = port;
            owner.incoming = incoming;
            owner.invalidate_after_intent = Some(disconnect);
            let waiting = owner
                .port
                .submit_confined_execution(&scope, &base.join("runtime/helper"), "touch new")
                .unwrap();
            owner.step(&mut writer, &target).unwrap();
            assert!(owner.awaiting.is_empty());
            assert!(owner.approval_events.is_empty());
            assert_eq!(
                waiting.await.unwrap().end,
                ControlledEnd::CancelledBeforeSpawn
            );
            assert!(!base.join("workspace/new").exists());
            let saved = writer.snapshot().unwrap();
            assert_eq!(saved.state.runs[0].operations.len(), 1);
            assert!(saved.state.runs[0].operations[0].result_id.is_some());
            assert!(owner.quiescent());
            writer
                .finish(
                    &target,
                    Observation::Cancelled,
                    ResultId::new("execution-cancelled").unwrap(),
                )
                .unwrap();
            owner.saved = true;
        }
    }

    #[test]
    fn trusted_read_existing_intent_never_replays() {
        let (_root, mut writer, target, mut owner, _dir) = read_owner();
        writer
            .record_intent(&target, OperationId::new("exec-read-run-1").unwrap())
            .unwrap();
        let _waiting = owner
            .port
            .submit_confined_read(&ConfinedReadRequest::DiffBefore {
                path: "dummy".into(),
            })
            .unwrap();
        assert!(owner.step(&mut writer, &target).is_err());
        assert!(owner.slots.is_empty());
        assert!(owner.awaiting.is_empty());
        assert_eq!(writer.snapshot().unwrap().state.runs[0].operations.len(), 1);
    }
    #[test]
    fn trusted_read_cancel_before_intent_and_ordinary_submit_still_require_approval() {
        let (_root, mut writer, target, mut owner, dir) = read_owner();
        let waiting = owner
            .port
            .submit_confined_read(&ConfinedReadRequest::DiffBefore {
                path: "dummy".into(),
            })
            .unwrap();
        drop(waiting);
        owner.step(&mut writer, &target).unwrap();
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
        let base = dir.path().canonicalize().unwrap();
        let policy = SandboxPolicy::isolated(
            SandboxMode::ReadOnly,
            &base.join("workspace"),
            &[base.join("runtime")],
        )
        .unwrap()
        .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
        .unwrap();
        let _waiting = owner
            .port
            .submit(ExecutionCommand {
                policy,
                program: base.join("runtime/helper"),
                args: vec!["--confined-read".into()],
                stdin: Some(
                    serde_json::to_string(&ConfinedReadRequest::DiffBefore {
                        path: "dummy".into(),
                    })
                    .unwrap(),
                ),
            })
            .unwrap();
        owner.step(&mut writer, &target).unwrap();
        assert_eq!(owner.awaiting.len(), 1);
        assert!(owner.slots.is_empty());
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
    }
    #[test]
    fn service_slot_capacity_is_reserved_until_owner_releases_it() {
        const MARKER: &str = "POLARIS_SERVICE_CAPACITY_FIXTURE";
        if std::env::var_os(MARKER).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "execution_owner::tests::service_slot_capacity_is_reserved_until_owner_releases_it",
                    "--nocapture",
                ])
                .env_clear()
                .env(MARKER, "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        let mut slots: Vec<_> = (0..CAPACITY)
            .map(|_| Reservation::acquire().unwrap())
            .collect();
        assert!(Reservation::acquire().is_none());
        slots.pop();
        let restored = Reservation::acquire().unwrap();
        assert!(Reservation::acquire().is_none());
        drop((restored, slots));
        assert_eq!(RESERVED.load(Ordering::Acquire), 0);
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "execution_owner/prepared_tests.rs"]
mod prepared_tests;
