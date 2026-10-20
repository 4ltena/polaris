//! Root delegation control, durable publication, runtime aborts and safe projections.

#[cfg(test)]
use super::Session;
use polaris_provider::{Message, ToolCall};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::subagent_control::{
    CancelPlanV1, CancelResultV1, CapabilitySnapshotV1, ChildAttemptId, ChildAttemptRefV1,
    ChildBindingV1, ChildControlPlaneV1, ChildStateV1, LogicalChildId, LogicalChildV1,
    ParentBindingV1, RootAttemptId, RootRunId, SafeActivityV1, TaskId,
};

/// Host-issued coordinates and authority for one root run.  No field is
/// derived from model text or a spawn tool argument.
#[derive(Debug, Clone)]
pub struct RootDelegationConfigV1 {
    pub session_id: String,
    pub root_run_id: RootRunId,
    pub root_attempt_id: RootAttemptId,
    pub active_task_id: TaskId,
    pub parent_capabilities: CapabilitySnapshotV1,
    pub model: String,
    pub effort: String,
}

/// Host-local reservation connecting one completed parent MCP read to exactly
/// one upcoming child. It is deliberately not serializable and never enters a
/// model tool argument, MCP reply, Desktop projection, or spawn request.
#[derive(Debug, Clone)]
pub(crate) struct ArmedChildMcpCaptureV1 {
    root_run_id: RootRunId,
    task_id: TaskId,
    intent: crate::mcp::lease::HostCaptureIntentV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildControlProjectionV1 {
    pub logical_child_id: LogicalChildId,
    pub attempt_id: ChildAttemptId,
    pub display_name: String,
    pub role: String,
    pub tier: String,
    pub model: String,
    pub effort: String,
    pub state: ChildStateV1,
    /// Latest bounded activity for this exact attempt.  It contains neither
    /// task prose nor tool arguments.
    pub activity: Option<SafeActivityV1>,
}

/// A revision-bound, secret-free child snapshot for the root host's wait
/// operation.  A missing value from `wait_children` means that its supplied
/// cursor is still current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildWaitSnapshotV1 {
    pub revision: u64,
    pub cursor: u64,
    pub children: Vec<ChildControlProjectionV1>,
}

/// The durable acknowledgement for one exact child cancellation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildCancelAckV1 {
    pub reference: ChildAttemptRefV1,
    pub revision: u64,
    pub result: CancelResultV1,
    pub runtime_effect: RuntimeCancelEffectV1,
}

/// Process-local delivery result for a durable cancellation request. Runtime
/// handles are intentionally absent after restore, so `Unavailable` is a
/// truthful terminal answer for that delivery attempt rather than a claim
/// that the child stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCancelEffectV1 {
    Unavailable,
    Requested,
}

/// The durable acknowledgement for a root-wide cancellation request.  The
/// plan retains the exact attempts that were eligible when the request began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCancelAckV1 {
    pub revision: u64,
    pub plan: CancelPlanV1,
    pub children: Vec<ChildCancelAckV1>,
}

/// A bounded, secret-free copy for persistence/event adapters.  It excludes
/// task prose, activity text, write paths, tool arguments, source identities,
/// payload hashes and MCP response text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDelegationProjectionV1 {
    pub revision: u64,
    pub session_id: String,
    pub root_run_id: RootRunId,
    pub root_attempt_id: RootAttemptId,
    pub active_task_id: TaskId,
    pub children: Vec<ChildControlProjectionV1>,
    pub mcp_leases: Vec<crate::mcp::lease::McpLeaseProjectionV1>,
}

pub type DelegationProjectionHookV1 = Arc<dyn Fn(RootDelegationProjectionV1) + Send + Sync>;

/// Exact host-owned coordinates for the durable control plane. This binding
/// is private host input, never model text or a wire/UI projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDelegationHostBindingV1 {
    pub session_id: String,
    pub root_run_id: RootRunId,
    pub root_attempt_id: RootAttemptId,
    pub active_task_id: TaskId,
}

/// Full owner checkpoint for a trusted persistence adapter. Unlike
/// [`RootDelegationProjectionV1`], this value must never cross a protocol or
/// UI boundary because it contains the complete child control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDelegationCheckpointV1 {
    pub expected_revision: u64,
    pub revision: u64,
    pub binding: RootDelegationHostBindingV1,
    pub control_plane: ChildControlPlaneV1,
}

pub type DelegationPersistenceHookV1 =
    Arc<dyn Fn(RootDelegationCheckpointV1) -> Result<(), String> + Send + Sync>;

#[derive(Debug, Default)]
struct DelegationPublicationGateV1 {
    publishing_thread: Option<std::thread::ThreadId>,
}

/// Root-local owner for child identity, state and read-only MCP evidence.
/// The live control plane remains private; consumers receive projections.
pub struct RootDelegationControlV1 {
    config: RootDelegationConfigV1,
    plane: Arc<tokio::sync::Mutex<ChildControlPlaneV1>>,
    leases: Arc<crate::mcp::lease::McpReadLeaseOwnerV1>,
    next_child: AtomicU64,
    next_task: AtomicU64,
    reserved_tasks: Mutex<BTreeSet<TaskId>>,
    issued_tasks: Mutex<BTreeSet<TaskId>>,
    next_spawn_tasks: Mutex<Option<Vec<TaskId>>>,
    runtime_attempts: Mutex<BTreeMap<(String, String), futures_util::future::AbortHandle>>,
    revision: AtomicU64,
    hook: RwLock<Option<DelegationProjectionHookV1>>,
    persistence_hook: RwLock<Option<DelegationPersistenceHookV1>>,
    publication_gate: Mutex<DelegationPublicationGateV1>,
    publication_ready: Condvar,
    transition_gate: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for RootDelegationControlV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RootDelegationControlV1")
            .field("session_id", &self.config.session_id)
            .field("root_run_id", &self.config.root_run_id)
            .field("root_attempt_id", &self.config.root_attempt_id)
            .field("active_task_id", &self.config.active_task_id)
            .finish_non_exhaustive()
    }
}

impl RootDelegationControlV1 {
    pub fn new(config: RootDelegationConfigV1) -> Result<Arc<Self>, String> {
        Self::build(config, ChildControlPlaneV1::default(), 0)
    }

    fn build(
        config: RootDelegationConfigV1,
        plane: ChildControlPlaneV1,
        revision: u64,
    ) -> Result<Arc<Self>, String> {
        if config.session_id.is_empty()
            || config.session_id.len() > 128
            || config.session_id.chars().any(char::is_control)
            || config.model.is_empty()
            || config.model.len() > 128
            || config.effort.is_empty()
            || config.effort.len() > 64
        {
            return Err("invalid root delegation configuration".into());
        }
        plane.validate().map_err(|error| error.to_string())?;
        if plane.children.iter().any(|child| {
            child.binding.root_run_id != config.root_run_id
                || child.binding.root_attempt_id != config.root_attempt_id
                || child.binding.active_task_id != config.active_task_id
        }) {
            return Err("restored delegation control has a foreign binding".into());
        }
        let leases = Arc::new(crate::mcp::lease::McpReadLeaseOwnerV1::new(
            config.root_run_id.clone(),
        ));
        let next_child = (plane.children.len() as u64)
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "restored delegation child counter overflow".to_owned())?;
        Ok(Arc::new(Self {
            config,
            plane: Arc::new(tokio::sync::Mutex::new(plane)),
            leases,
            next_child: AtomicU64::new(next_child),
            next_task: AtomicU64::new(1),
            reserved_tasks: Mutex::new(BTreeSet::new()),
            issued_tasks: Mutex::new(BTreeSet::new()),
            next_spawn_tasks: Mutex::new(None),
            runtime_attempts: Mutex::new(BTreeMap::new()),
            revision: AtomicU64::new(revision),
            hook: RwLock::new(None),
            persistence_hook: RwLock::new(None),
            publication_gate: Mutex::new(DelegationPublicationGateV1::default()),
            publication_ready: Condvar::new(),
            transition_gate: Arc::new(tokio::sync::Mutex::new(())),
        }))
    }

    /// Construct a new root and install the trusted durable owner before any
    /// child state can be issued. The hook receives revision 0 -> 1 first.
    pub fn new_with_persistence_hook(
        config: RootDelegationConfigV1,
        hook: DelegationPersistenceHookV1,
    ) -> Result<Arc<Self>, String> {
        let owner = Self::new(config)?;
        owner.install_persistence_hook(0, hook)?;
        Ok(owner)
    }

    /// Restore a validated durable plane. Leases are intentionally not
    /// restored; a restarted process must obtain a new host attestation and
    /// new exact child-attempt binding before sharing any MCP text.
    pub fn restore_with_persistence_hook(
        config: RootDelegationConfigV1,
        mut plane: ChildControlPlaneV1,
        revision: u64,
        hook: DelegationPersistenceHookV1,
    ) -> Result<Arc<Self>, String> {
        let previous_cursor = plane.cursor;
        plane.outcome_unknown_on_restore();
        let changed = plane.cursor != previous_cursor;
        let owner = Self::build(config, plane.clone(), revision)?;
        owner.install_persistence_hook(revision, hook)?;
        if changed {
            owner.notify(&plane)?;
        }
        Ok(owner)
    }

    /// Build an in-process live control plane for transport tests. Production
    /// restore must continue to turn every nonterminal attempt into
    /// `OutcomeUnknown`; this helper exists only so a service fixture can own
    /// a genuinely running attempt and its process-local abort handle.
    #[cfg(feature = "execution-test-support")]
    pub fn new_live_with_persistence_hook_for_test(
        config: RootDelegationConfigV1,
        plane: ChildControlPlaneV1,
        revision: u64,
        hook: DelegationPersistenceHookV1,
    ) -> Result<Arc<Self>, String> {
        let owner = Self::build(config, plane, revision)?;
        owner.install_persistence_hook(revision, hook)?;
        Ok(owner)
    }

    /// Install the host-only CAS adapter at the exact current revision.
    /// Installation is single-assignment so a live root cannot switch its
    /// durable owner while children are running.
    pub fn install_persistence_hook(
        &self,
        expected_revision: u64,
        hook: DelegationPersistenceHookV1,
    ) -> Result<(), String> {
        if self.revision.load(Ordering::Acquire) != expected_revision {
            return Err("delegation persistence revision mismatch".into());
        }
        let mut held = self
            .persistence_hook
            .write()
            .map_err(|_| "delegation persistence hook unavailable".to_owned())?;
        if held.is_some() {
            return Err("delegation persistence hook is already installed".into());
        }
        *held = Some(hook);
        Ok(())
    }

    pub fn set_projection_hook(&self, hook: Option<DelegationProjectionHookV1>) {
        if let Ok(mut held) = self.hook.write() {
            *held = hook;
        }
    }

    pub async fn snapshot(&self) -> RootDelegationProjectionV1 {
        let plane = self.plane.lock().await.clone();
        self.projection_from(&plane, self.revision.load(Ordering::Acquire))
    }

    /// Return the latest safe child state only when the caller's cursor is
    /// stale.  The host binding and revision prevent a foreign or delayed UI
    /// request from observing another root's control plane.
    pub async fn wait_children(
        &self,
        binding: &RootDelegationHostBindingV1,
        expected_revision: u64,
        after_cursor: Option<u64>,
    ) -> Result<Option<ChildWaitSnapshotV1>, String> {
        let _transition = self.transition_gate.lock().await;
        self.require_host_request(binding, expected_revision)?;
        let plane = self.plane.lock().await;
        Ok(plane
            .wait_snapshot(after_cursor)
            .map(|snapshot| ChildWaitSnapshotV1 {
                revision: expected_revision,
                cursor: snapshot.cursor,
                children: self.child_projections(&snapshot.children),
            }))
    }

    /// Record a cancellation intent durably, then deliver the process-local
    /// effect to the exact attempt if its handle still exists.
    pub async fn request_child_cancel(
        &self,
        binding: &RootDelegationHostBindingV1,
        expected_revision: u64,
        reference: ChildAttemptRefV1,
    ) -> Result<ChildCancelAckV1, String> {
        let _transition = self.transition_gate.lock().await;
        self.require_host_request(binding, expected_revision)?;
        let child_binding = self.child_binding_for_request();
        let mut plane = self.plane.lock().await;
        let before = plane.clone();
        let result = plane
            .cancel_requested(&reference, &child_binding)
            .map_err(|error| error.to_string())?;
        let changed = matches!(&result, CancelResultV1::Requested { .. });
        let after = plane.clone();
        drop(plane);
        let revision = if changed {
            if let Err(error) = self.notify(&after) {
                *self.plane.lock().await = before;
                return Err(error);
            }
            self.revision.load(Ordering::Acquire)
        } else {
            expected_revision
        };
        let runtime_effect = if matches!(&result, CancelResultV1::AlreadyTerminal { .. }) {
            RuntimeCancelEffectV1::Unavailable
        } else {
            self.abort_registered_attempt(&reference)
        };
        Ok(ChildCancelAckV1 {
            reference,
            revision,
            result,
            runtime_effect,
        })
    }

    /// Durably request cancellation of all nonterminal attempts belonging to
    /// this exact root attempt, then deliver effects to only the handles named
    /// by that persisted plan.
    pub async fn request_root_cancel(
        &self,
        binding: &RootDelegationHostBindingV1,
        expected_revision: u64,
    ) -> Result<RootCancelAckV1, String> {
        let _transition = self.transition_gate.lock().await;
        self.require_host_request(binding, expected_revision)?;
        let child_binding = self.child_binding_for_request();
        let mut plane = self.plane.lock().await;
        let before = plane.clone();
        let plan =
            plane.cancel_plan_for_root(&self.config.root_run_id, &self.config.root_attempt_id);
        let mut children = Vec::with_capacity(plan.targets.len());
        for reference in &plan.targets {
            let result = match plane.cancel_requested(reference, &child_binding) {
                Ok(result) => result,
                Err(error) => {
                    *plane = before;
                    return Err(error.to_string());
                }
            };
            children.push(ChildCancelAckV1 {
                reference: reference.clone(),
                revision: expected_revision,
                result,
                runtime_effect: RuntimeCancelEffectV1::Unavailable,
            });
        }
        let changed = children
            .iter()
            .any(|child| matches!(&child.result, CancelResultV1::Requested { .. }));
        let after = plane.clone();
        drop(plane);
        let revision = if changed {
            if let Err(error) = self.notify(&after) {
                *self.plane.lock().await = before;
                return Err(error);
            }
            self.revision.load(Ordering::Acquire)
        } else {
            expected_revision
        };
        for child in &mut children {
            child.revision = revision;
            if !matches!(&child.result, CancelResultV1::AlreadyTerminal { .. }) {
                child.runtime_effect = self.abort_registered_attempt(&child.reference);
            }
        }
        Ok(RootCancelAckV1 {
            revision,
            plan,
            children,
        })
    }

    fn runtime_key(reference: &ChildAttemptRefV1) -> (String, String) {
        (
            reference.logical_child_id.0.clone(),
            reference.attempt_id.0.clone(),
        )
    }

    fn abort_registered_attempt(&self, reference: &ChildAttemptRefV1) -> RuntimeCancelEffectV1 {
        let handle = self
            .runtime_attempts
            .lock()
            .ok()
            .and_then(|mut attempts| attempts.remove(&Self::runtime_key(reference)));
        match handle {
            Some(handle) => {
                handle.abort();
                RuntimeCancelEffectV1::Requested
            }
            None => RuntimeCancelEffectV1::Unavailable,
        }
    }

    /// Register one process-local future only after its exact attempt is
    /// durably Running. The handle never enters checkpoints and therefore
    /// cannot be revived or confused with a restored attempt.
    pub(crate) async fn register_runtime_attempt(
        &self,
        reference: &ChildAttemptRefV1,
        handle: futures_util::future::AbortHandle,
    ) -> Result<(), String> {
        let _transition = self.transition_gate.lock().await;
        let plane = self.plane.lock().await;
        let running = plane.children.iter().any(|child| {
            child.logical_child_id == reference.logical_child_id
                && child.binding.root_run_id == self.config.root_run_id
                && child.binding.root_attempt_id == self.config.root_attempt_id
                && child.attempts.iter().any(|attempt| {
                    attempt.attempt_id == reference.attempt_id
                        && attempt.state == ChildStateV1::Running
                })
        });
        drop(plane);
        if !running {
            return Err("delegation runtime attempt is not durably running".into());
        }
        let mut attempts = self
            .runtime_attempts
            .lock()
            .map_err(|_| "delegation runtime registry unavailable".to_owned())?;
        let key = Self::runtime_key(reference);
        if attempts.contains_key(&key) {
            return Err("delegation runtime attempt is already registered".into());
        }
        attempts.insert(key, handle);
        Ok(())
    }

    /// Test-only access to the same exact-attempt registry used by controlled
    /// spawn futures. Product callers cannot manufacture runtime handles.
    #[cfg(feature = "execution-test-support")]
    pub async fn install_runtime_attempt_for_test(
        &self,
        reference: &ChildAttemptRefV1,
    ) -> Result<futures_util::future::AbortHandle, String> {
        let (handle, _) = futures_util::future::AbortHandle::new_pair();
        let observer = handle.clone();
        self.register_runtime_attempt(reference, handle).await?;
        Ok(observer)
    }

    pub(crate) fn unregister_runtime_attempt(
        &self,
        reference: &ChildAttemptRefV1,
    ) -> Result<(), String> {
        self.runtime_attempts
            .lock()
            .map_err(|_| "delegation runtime registry unavailable".to_owned())?
            .remove(&Self::runtime_key(reference));
        Ok(())
    }

    /// An abort signal cannot prove that the child had not already sent a
    /// provider request or started a tool. Persist the only honest terminal
    /// state after the aborted future yields.
    pub(crate) async fn finish_runtime_abort_unknown(
        &self,
        reference: &ChildAttemptRefV1,
    ) -> Result<(), String> {
        let _transition = self.transition_gate.lock().await;
        self.unregister_runtime_attempt(reference)?;
        let binding = self.child_binding_for_request();
        let mut plane = self.plane.lock().await;
        let before = plane.clone();
        let result = plane.finish(
            reference,
            &binding,
            ChildStateV1::OutcomeUnknown,
            "runtime cancellation was delivered after the child started".into(),
            crate::subagent_control::UsageAttributionV1::Missing,
            Vec::new(),
            Vec::new(),
        );
        match result {
            Ok(_) => {}
            Err(crate::subagent_control::ControlError::Conflict) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
        let after = plane.clone();
        drop(plane);
        if let Err(error) = self.notify(&after) {
            *self.plane.lock().await = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn capture_parent_mcp_read(
        &self,
        attestation: &crate::mcp::lease::ShareableTextAttestationV1,
        call: &ToolCall,
        message: &Message,
    ) -> Result<bool, crate::mcp::lease::LeaseError> {
        let reserved = self
            .reserved_tasks
            .lock()
            .map_err(|_| crate::mcp::lease::LeaseError::Unavailable)?;
        if !reserved.contains(&attestation.intended_task_id) {
            return Err(crate::mcp::lease::LeaseError::WrongBinding);
        }
        drop(reserved);
        self.leases
            .capture_published_read(attestation, call, message, unix_time_ms())
    }

    /// Reserve and arm one exact child target before a host chooses to retain
    /// a parent MCP reply. The task selector is generated here and remains in
    /// this private handle; model text, MCP output and spawn arguments cannot
    /// nominate a child recipient.
    pub(crate) fn arm_child_mcp_capture(
        &self,
    ) -> Result<ArmedChildMcpCaptureV1, crate::mcp::lease::LeaseError> {
        let task_id = self
            .reserve_child_task_id()
            .map_err(|_| crate::mcp::lease::LeaseError::Unavailable)?;
        let intent = self
            .leases
            .arm_reserved_capture(task_id.clone(), unix_time_ms())?;
        Ok(ArmedChildMcpCaptureV1 {
            root_run_id: self.config.root_run_id.clone(),
            task_id,
            intent,
        })
    }

    /// Runs only after the canonical tool result was appended and its durable
    /// owner acknowledged that append. An invalid/binary/retained reply is
    /// not leaseable; the consumed intent cannot be replayed later.
    pub(crate) fn capture_armed_parent_mcp_read(
        &self,
        armed: &ArmedChildMcpCaptureV1,
        call: &ToolCall,
        message: &Message,
    ) -> Result<bool, crate::mcp::lease::LeaseError> {
        self.require_armed_capture(armed)?;
        self.leases
            .capture_armed_published_read(&armed.intent, call, message, unix_time_ms())
    }

    /// Binds an armed target to the immediately following one-child spawn.
    /// A multi-child wave cannot receive a read intended for only one task and
    /// therefore fails closed without exposing the reservation to the model.
    pub(crate) fn bind_armed_capture_to_next_spawn(
        &self,
        armed: &ArmedChildMcpCaptureV1,
        child_count: usize,
    ) -> Result<(), String> {
        self.require_armed_capture(armed)
            .map_err(|error| error.to_string())?;
        if child_count != 1 {
            return Err("armed MCP capture requires exactly one child".into());
        }
        self.bind_next_spawn_tasks(vec![armed.task_id.clone()])
    }

    /// Drops both an unused intent and any unbound redacted body for this
    /// exact task. This is called for unrelated actions, rejected spawn
    /// requests and turn teardown.
    pub(crate) fn discard_armed_child_mcp_capture(
        &self,
        armed: &ArmedChildMcpCaptureV1,
    ) -> Result<usize, crate::mcp::lease::LeaseError> {
        self.require_armed_capture(armed)?;
        let intent = usize::from(self.leases.discard_capture_intent(&armed.intent));
        let pending = self.leases.discard_pending_for_task(&armed.task_id);
        Ok(intent.saturating_add(pending))
    }

    fn require_armed_capture(
        &self,
        armed: &ArmedChildMcpCaptureV1,
    ) -> Result<(), crate::mcp::lease::LeaseError> {
        if armed.root_run_id != self.config.root_run_id {
            return Err(crate::mcp::lease::LeaseError::WrongBinding);
        }
        let reserved = self
            .reserved_tasks
            .lock()
            .map_err(|_| crate::mcp::lease::LeaseError::Unavailable)?;
        if reserved.contains(&armed.task_id) {
            Ok(())
        } else {
            Err(crate::mcp::lease::LeaseError::WrongBinding)
        }
    }

    pub async fn revoke_root_terminal(&self) -> usize {
        let revoked = self.leases.revoke_root_terminal();
        let plane = self.plane.lock().await.clone();
        self.notify_projection_only(&plane);
        revoked
    }

    pub(crate) fn issue_spawn_port(
        self: &Arc<Self>,
        count: usize,
        parent: ParentBindingV1,
    ) -> Result<crate::spawn::SpawnControlPort, String> {
        if count == 0 || count > 128 {
            return Err("invalid controlled spawn wave size".into());
        }
        let task_ids = match self
            .next_spawn_tasks
            .lock()
            .map_err(|_| "delegation task binding queue unavailable".to_owned())?
            .take()
        {
            Some(task_ids) if task_ids.len() == count => task_ids,
            Some(_) => return Err("prebound child task count does not match spawn wave".into()),
            None => (0..count)
                .map(|_| self.reserve_child_task_id())
                .collect::<Result<Vec<_>, _>>()?,
        };
        self.issue_spawn_port_for_tasks(task_ids, parent)
    }

    /// Reserve a selector before a host policy captures shareable MCP text.
    /// The selector is random from the model's perspective and cannot be
    /// supplied through a tool argument.
    pub fn reserve_child_task_id(&self) -> Result<TaskId, String> {
        let ordinal = self.next_task.fetch_add(1, Ordering::Relaxed);
        let root_key =
            crate::conversation_state::content_hash(self.config.root_run_id.0.as_bytes());
        let task_id = TaskId::new(format!("child-task.{}.{ordinal}", &root_key[..16]))
            .map_err(|error| error.to_string())?;
        self.reserved_tasks
            .lock()
            .map_err(|_| "delegation task reservation registry unavailable".to_owned())?
            .insert(task_id.clone());
        Ok(task_id)
    }

    /// Bind host-reserved selectors to the next spawn wave. This is the only
    /// route by which an attested capture can reach a child task.
    pub fn bind_next_spawn_tasks(&self, task_ids: Vec<TaskId>) -> Result<(), String> {
        if task_ids.is_empty()
            || task_ids.len() > 128
            || task_ids.iter().collect::<BTreeSet<_>>().len() != task_ids.len()
        {
            return Err("invalid prebound child task set".into());
        }
        let reserved = self
            .reserved_tasks
            .lock()
            .map_err(|_| "delegation task reservation registry unavailable".to_owned())?;
        if task_ids.iter().any(|task| !reserved.contains(task)) {
            return Err("prebound child task was not reserved by this root".into());
        }
        drop(reserved);
        let issued = self
            .issued_tasks
            .lock()
            .map_err(|_| "delegation task registry unavailable".to_owned())?;
        if task_ids.iter().any(|task| issued.contains(task)) {
            return Err("prebound child task was already issued".into());
        }
        drop(issued);
        let mut next = self
            .next_spawn_tasks
            .lock()
            .map_err(|_| "delegation task binding queue unavailable".to_owned())?;
        if next.is_some() {
            return Err("a child task set is already bound to the next spawn".into());
        }
        *next = Some(task_ids);
        Ok(())
    }

    pub(crate) fn issue_spawn_port_for_tasks(
        self: &Arc<Self>,
        task_ids: Vec<TaskId>,
        parent: ParentBindingV1,
    ) -> Result<crate::spawn::SpawnControlPort, String> {
        if task_ids.is_empty() || task_ids.len() > 128 {
            return Err("invalid controlled spawn task binding count".into());
        }
        {
            let reserved = self
                .reserved_tasks
                .lock()
                .map_err(|_| "delegation task reservation registry unavailable".to_owned())?;
            if task_ids.iter().any(|task| !reserved.contains(task)) {
                return Err("delegation task binding was not reserved by this root".into());
            }
            drop(reserved);
            let mut issued = self
                .issued_tasks
                .lock()
                .map_err(|_| "delegation task registry unavailable".to_owned())?;
            if task_ids.iter().any(|task| issued.contains(task))
                || task_ids.iter().collect::<BTreeSet<_>>().len() != task_ids.len()
            {
                return Err("delegation task binding was already issued".into());
            }
            issued.extend(task_ids.iter().cloned());
        }
        let count = task_ids.len();
        let first = self
            .next_child
            .fetch_add((count as u64).saturating_mul(2), Ordering::Relaxed);
        let root_key =
            crate::conversation_state::content_hash(self.config.root_run_id.0.as_bytes());
        let children = task_ids
            .into_iter()
            .enumerate()
            .map(|(index, task_id)| {
                let ordinal = first.saturating_add((index as u64).saturating_mul(2));
                let prefix = format!("child.{}.{ordinal}", &root_key[..16]);
                Ok(crate::spawn::ControlledChildIds {
                    logical_child_id: LogicalChildId::new(prefix.clone())?,
                    task_id,
                    first_attempt_id: ChildAttemptId::new(format!("{prefix}.attempt.1"))?,
                    retry_attempt_id: ChildAttemptId::new(format!("{prefix}.attempt.2"))?,
                })
            })
            .collect::<Result<Vec<_>, crate::subagent_control::ControlError>>()
            .map_err(|error| error.to_string())?;
        let weak = Arc::downgrade(self);
        Ok(crate::spawn::SpawnControlPort {
            plane: self.plane.clone(),
            binding: ChildBindingV1 {
                root_run_id: self.config.root_run_id.clone(),
                root_attempt_id: self.config.root_attempt_id.clone(),
                active_task_id: self.config.active_task_id.clone(),
                parent,
            },
            parent_capabilities: self.config.parent_capabilities.clone(),
            model: self.config.model.clone(),
            effort: self.config.effort.clone(),
            children,
            retry_safe_transient: true,
            lease_owner: Some(self.leases.clone()),
            on_change: Some(Arc::new(move |plane| {
                if let Some(owner) = weak.upgrade() {
                    owner.notify(plane)
                } else {
                    Err("root delegation owner is unavailable".into())
                }
            })),
            root_owner: Some(self.clone()),
            transition_gate: self.transition_gate.clone(),
        })
    }

    fn notify(&self, plane: &ChildControlPlaneV1) -> Result<(), String> {
        plane.validate().map_err(|error| error.to_string())?;
        let current_thread = std::thread::current().id();
        let mut gate = self
            .publication_gate
            .lock()
            .map_err(|_| "delegation publication gate unavailable".to_owned())?;
        while let Some(publishing_thread) = gate.publishing_thread {
            if publishing_thread == current_thread {
                return Err("recursive delegation persistence callback refused".into());
            }
            gate = self
                .publication_ready
                .wait(gate)
                .map_err(|_| "delegation publication gate unavailable".to_owned())?;
        }
        gate.publishing_thread = Some(current_thread);
        drop(gate);

        let result = (|| -> Result<(), String> {
            let expected_revision = self.revision.load(Ordering::Acquire);
            let revision = expected_revision
                .checked_add(1)
                .ok_or_else(|| "delegation persistence revision overflow".to_owned())?;
            let checkpoint = RootDelegationCheckpointV1 {
                expected_revision,
                revision,
                binding: self.host_binding(),
                control_plane: plane.clone(),
            };
            let persistence_hook = self
                .persistence_hook
                .read()
                .map_err(|_| "delegation persistence hook unavailable".to_owned())?
                .clone();
            if let Some(hook) = persistence_hook {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(checkpoint)))
                    .map_err(|_| "delegation persistence hook panicked".to_owned())??;
            }
            self.revision.store(revision, Ordering::Release);
            self.notify_projection(plane, revision);
            Ok(())
        })();

        if let Ok(mut gate) = self.publication_gate.lock() {
            gate.publishing_thread = None;
            self.publication_ready.notify_all();
        }
        result
    }

    fn notify_projection_only(&self, plane: &ChildControlPlaneV1) {
        self.notify_projection(plane, self.revision.load(Ordering::Acquire));
    }

    fn notify_projection(&self, plane: &ChildControlPlaneV1, revision: u64) {
        let projection = self.projection_from(plane, revision);
        let hook = self.hook.read().ok().and_then(|held| held.clone());
        if let Some(hook) = hook {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(projection)));
        }
    }

    /// Exact host coordinates required by wait and cancellation requests.
    /// This contains no model-controlled text and is safe to retain beside
    /// the root owner for the lifetime of one host run.
    pub fn host_binding(&self) -> RootDelegationHostBindingV1 {
        RootDelegationHostBindingV1 {
            session_id: self.config.session_id.clone(),
            root_run_id: self.config.root_run_id.clone(),
            root_attempt_id: self.config.root_attempt_id.clone(),
            active_task_id: self.config.active_task_id.clone(),
        }
    }

    fn require_host_request(
        &self,
        binding: &RootDelegationHostBindingV1,
        expected_revision: u64,
    ) -> Result<(), String> {
        if *binding != self.host_binding() {
            return Err("delegation host binding mismatch".into());
        }
        if self.revision.load(Ordering::Acquire) != expected_revision {
            return Err("delegation control revision mismatch".into());
        }
        Ok(())
    }

    fn child_binding_for_request(&self) -> ChildBindingV1 {
        ChildBindingV1 {
            root_run_id: self.config.root_run_id.clone(),
            root_attempt_id: self.config.root_attempt_id.clone(),
            active_task_id: self.config.active_task_id.clone(),
            parent: ParentBindingV1::Root,
        }
    }

    fn child_projections(&self, children: &[LogicalChildV1]) -> Vec<ChildControlProjectionV1> {
        children
            .iter()
            .filter_map(|child| {
                child
                    .attempts
                    .last()
                    .map(|attempt| ChildControlProjectionV1 {
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
            .collect()
    }

    fn projection_from(
        &self,
        plane: &ChildControlPlaneV1,
        revision: u64,
    ) -> RootDelegationProjectionV1 {
        let children = self.child_projections(&plane.children);
        RootDelegationProjectionV1 {
            revision,
            session_id: self.config.session_id.clone(),
            root_run_id: self.config.root_run_id.clone(),
            root_attempt_id: self.config.root_attempt_id.clone(),
            active_task_id: self.config.active_task_id.clone(),
            children,
            mcp_leases: self.leases.projection(unix_time_ms()),
        }
    }
}

pub(super) fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod root_delegation_host_control_tests {
    use super::*;

    fn config() -> RootDelegationConfigV1 {
        RootDelegationConfigV1 {
            session_id: "session-1".into(),
            root_run_id: RootRunId::new("root-1").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt-1").unwrap(),
            active_task_id: TaskId::new("task-1").unwrap(),
            parent_capabilities: CapabilitySnapshotV1 {
                sandbox: crate::subagent_control::SandboxCapabilityV1::ReadOnly,
                write_roots: vec![],
                tool_names: vec!["read".into(), "spawn".into()],
                approval: crate::subagent_control::ApprovalCapabilityV1::Never,
                mcp: crate::subagent_control::McpCapabilityV1::ReadSnapshotsOnly,
            },
            model: "gpt-6-astra".into(),
            effort: "xhigh".into(),
        }
    }

    async fn running_child(
        control: &Arc<RootDelegationControlV1>,
        suffix: &str,
    ) -> ChildAttemptRefV1 {
        let reference = ChildAttemptRefV1 {
            logical_child_id: LogicalChildId::new(format!("child-{suffix}")).unwrap(),
            attempt_id: ChildAttemptId::new(format!("attempt-{suffix}")).unwrap(),
        };
        let binding = control.child_binding_for_request();
        let mut plane = control.plane.lock().await;
        plane
            .register_before_start(crate::subagent_control::RegisterChildV1 {
                logical_child_id: reference.logical_child_id.clone(),
                attempt_id: reference.attempt_id.clone(),
                binding: binding.clone(),
                role: "file-reader".into(),
                tier: "dwarf".into(),
                model: "gpt-6-astra".into(),
                effort: "xhigh".into(),
                capabilities: config().parent_capabilities,
            })
            .unwrap();
        plane.mark_running(&reference, &binding).unwrap();
        reference
    }

    #[tokio::test]
    async fn wait_children_exposes_only_latest_safe_attempt_activity() {
        let control = RootDelegationControlV1::new(config()).unwrap();
        let reference = running_child(&control, "one").await;
        {
            let mut plane = control.plane.lock().await;
            plane
                .mark_activity(
                    &reference,
                    &control.child_binding_for_request(),
                    SafeActivityV1 {
                        summary: "read(Cargo.toml)".into(),
                        sequence: 4,
                    },
                )
                .unwrap();
        }
        let binding = control.host_binding();
        let wait = control
            .wait_children(&binding, 0, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wait.children.len(), 1);
        assert_eq!(wait.children[0].attempt_id, reference.attempt_id);
        assert_eq!(wait.children[0].activity.as_ref().unwrap().sequence, 4);
        assert!(
            control
                .wait_children(&binding, 0, Some(wait.cursor))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn child_cancel_requires_exact_binding_and_durable_current_revision() {
        let checkpoints = Arc::new(Mutex::new(Vec::<RootDelegationCheckpointV1>::new()));
        let captured = checkpoints.clone();
        let control = RootDelegationControlV1::new_with_persistence_hook(
            config(),
            Arc::new(move |checkpoint| {
                captured.lock().unwrap().push(checkpoint);
                Ok(())
            }),
        )
        .unwrap();
        let reference = running_child(&control, "one").await;
        let mut foreign = control.host_binding();
        foreign.session_id = "other".into();
        assert!(
            control
                .request_child_cancel(&foreign, 0, reference.clone())
                .await
                .is_err()
        );
        let binding = control.host_binding();
        let ack = control
            .request_child_cancel(&binding, 0, reference.clone())
            .await
            .unwrap();
        assert!(matches!(&ack.result, CancelResultV1::Requested { .. }));
        assert_eq!(ack.revision, 1);
        assert_eq!(ack.runtime_effect, RuntimeCancelEffectV1::Unavailable);
        {
            let saved = checkpoints.lock().unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].expected_revision, 0);
            assert_eq!(saved[0].revision, 1);
            assert_eq!(
                saved[0].control_plane.children[0].attempts[0].state,
                ChildStateV1::Cancelling
            );
        }
        assert!(
            control
                .request_child_cancel(&binding, 0, reference.clone())
                .await
                .is_err()
        );
        let repeated = control
            .request_child_cancel(&binding, 1, reference)
            .await
            .unwrap();
        assert!(matches!(
            repeated.result,
            CancelResultV1::AlreadyRequested { .. }
        ));
        assert_eq!(repeated.revision, 1);
    }

    #[tokio::test]
    async fn root_cancel_is_idempotent_and_excludes_terminal_children() {
        let control = RootDelegationControlV1::new(config()).unwrap();
        let running = running_child(&control, "running").await;
        let terminal = running_child(&control, "terminal").await;
        {
            let mut plane = control.plane.lock().await;
            plane
                .finish(
                    &terminal,
                    &control.child_binding_for_request(),
                    ChildStateV1::Succeeded,
                    "done".into(),
                    crate::subagent_control::UsageAttributionV1::Missing,
                    vec![],
                    vec![],
                )
                .unwrap();
        }
        let (runtime_handle, _) = futures_util::future::AbortHandle::new_pair();
        let runtime_observer = runtime_handle.clone();
        control
            .register_runtime_attempt(&running, runtime_handle)
            .await
            .unwrap();
        let binding = control.host_binding();
        let first = control.request_root_cancel(&binding, 0).await.unwrap();
        assert_eq!(first.plan.targets, vec![running.clone()]);
        assert!(matches!(
            first.children[0].result,
            CancelResultV1::Requested { .. }
        ));
        assert_eq!(first.revision, 1);
        assert_eq!(
            first.children[0].runtime_effect,
            RuntimeCancelEffectV1::Requested
        );
        assert!(runtime_observer.is_aborted());
        let repeated = control.request_root_cancel(&binding, 1).await.unwrap();
        assert_eq!(repeated.plan.targets, vec![running]);
        assert!(matches!(
            repeated.children[0].result,
            CancelResultV1::AlreadyRequested { .. }
        ));
        assert_eq!(repeated.revision, 1);
        assert_eq!(
            repeated.children[0].runtime_effect,
            RuntimeCancelEffectV1::Unavailable
        );
    }

    #[tokio::test]
    async fn durable_child_cancel_aborts_exact_runtime_and_converges_outcome_unknown() {
        let (handle, registration) = futures_util::future::AbortHandle::new_pair();
        let observer = handle.clone();
        let control = RootDelegationControlV1::new_with_persistence_hook(
            config(),
            Arc::new(move |checkpoint| {
                if checkpoint.revision == 1 {
                    assert!(
                        !observer.is_aborted(),
                        "runtime effect preceded durable cancellation"
                    );
                }
                Ok(())
            }),
        )
        .unwrap();
        let reference = running_child(&control, "runtime").await;
        control
            .register_runtime_attempt(&reference, handle)
            .await
            .unwrap();
        let runtime =
            futures_util::future::Abortable::new(std::future::pending::<()>(), registration);
        let binding = control.host_binding();
        let ack = control
            .request_child_cancel(&binding, 0, reference.clone())
            .await
            .unwrap();
        assert!(matches!(&ack.result, CancelResultV1::Requested { .. }));
        assert_eq!(ack.runtime_effect, RuntimeCancelEffectV1::Requested);
        assert!(runtime.await.is_err());

        control
            .finish_runtime_abort_unknown(&reference)
            .await
            .unwrap();
        let snapshot = control.snapshot().await;
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.children[0].state, ChildStateV1::OutcomeUnknown);
    }

    #[tokio::test]
    async fn restored_nonterminal_child_has_no_runtime_handle_and_stays_fail_closed() {
        let original = RootDelegationControlV1::new(config()).unwrap();
        let reference = running_child(&original, "restored").await;
        let plane = original.plane.lock().await.clone();
        let restored = RootDelegationControlV1::restore_with_persistence_hook(
            config(),
            plane,
            0,
            Arc::new(|_| Ok(())),
        )
        .unwrap();
        let snapshot = restored.snapshot().await;
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.children[0].state, ChildStateV1::OutcomeUnknown);

        let ack = restored
            .request_child_cancel(&restored.host_binding(), 1, reference)
            .await
            .unwrap();
        assert!(matches!(
            &ack.result,
            CancelResultV1::AlreadyTerminal { .. }
        ));
        assert_eq!(ack.runtime_effect, RuntimeCancelEffectV1::Unavailable);
    }

    #[tokio::test]
    async fn cancel_finish_race_keeps_cancelling_until_executor_confirms_safe_terminal_state() {
        let control = RootDelegationControlV1::new(config()).unwrap();
        let reference = running_child(&control, "one").await;
        let binding = control.host_binding();
        control
            .request_child_cancel(&binding, 0, reference.clone())
            .await
            .unwrap();
        let mut plane = control.plane.lock().await;
        assert!(
            plane
                .finish(
                    &reference,
                    &control.child_binding_for_request(),
                    ChildStateV1::Succeeded,
                    "late success".into(),
                    crate::subagent_control::UsageAttributionV1::Missing,
                    vec![],
                    vec![],
                )
                .is_err()
        );
        assert_eq!(
            plane.children[0].attempts[0].state,
            ChildStateV1::Cancelling
        );
        plane
            .finish(
                &reference,
                &control.child_binding_for_request(),
                ChildStateV1::OutcomeUnknown,
                "cancel raced with finish".into(),
                crate::subagent_control::UsageAttributionV1::Missing,
                vec![],
                vec![],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn interactive_root_finish_marks_interrupted_children_unknown_and_allows_next_turn() {
        let control = RootDelegationControlV1::new(config()).unwrap();
        let child = running_child(&control, "interrupt").await;
        let mut session = Session::new();
        session
            .install_root_delegation_control(control.clone())
            .unwrap();

        assert_eq!(
            session.finish_root_delegation_control(true).await.unwrap(),
            0
        );
        assert!(session.delegation_control.is_none());
        assert!(session.delegation_parent.is_none());
        assert!(session.leased_mcp_reads.is_none());
        let projection = control.snapshot().await;
        assert_eq!(projection.children.len(), 1);
        assert_eq!(
            projection.children[0].logical_child_id,
            child.logical_child_id
        );
        assert_eq!(projection.children[0].state, ChildStateV1::OutcomeUnknown);

        session
            .install_root_delegation_control(RootDelegationControlV1::new(config()).unwrap())
            .unwrap();
        session.finish_root_delegation_control(false).await.unwrap();
    }
    #[tokio::test]
    async fn failed_host_persistence_does_not_publish_a_revision() {
        let control = RootDelegationControlV1::new_with_persistence_hook(
            RootDelegationConfigV1 {
                session_id: "session-1".into(),
                root_run_id: RootRunId::new("root-1").unwrap(),
                root_attempt_id: RootAttemptId::new("root-attempt-1").unwrap(),
                active_task_id: TaskId::new("task-1").unwrap(),
                parent_capabilities: CapabilitySnapshotV1 {
                    sandbox: crate::subagent_control::SandboxCapabilityV1::ReadOnly,
                    write_roots: vec![],
                    tool_names: vec!["read".into(), "spawn".into()],
                    approval: crate::subagent_control::ApprovalCapabilityV1::Never,
                    mcp: crate::subagent_control::McpCapabilityV1::ReadSnapshotsOnly,
                },
                model: "gpt-6-astra".into(),
                effort: "xhigh".into(),
            },
            Arc::new(|_| Err("durable CAS refused".into())),
        )
        .unwrap();
        let plane = ChildControlPlaneV1::default();
        assert_eq!(control.notify(&plane), Err("durable CAS refused".into()));
        assert_eq!(control.snapshot().await.revision, 0);
    }
}
