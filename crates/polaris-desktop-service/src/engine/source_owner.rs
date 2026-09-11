//! Engine-owned source lifecycle. Only the I/O Job leaves the blocking owner;
//! Writer, permission consumption, cancellation and saved ACKs stay serialized.
use super::*;
use crate::{
    source_apply::{
        CurrentSourcePolicy, PreparedSourceRecovery, SourceApplyClock, SourceApplyController,
        SourceApplyError, SourceApplyRequest, SourceApplyState,
    },
    source_apply_io::{self, Job, Outcome, Work},
};
use polaris_core::isolated_workspace;
use polaris_desktop_protocol::snapshot::ApprovalDecision;

pub struct SourceApplyRuntime {
    pub factory: Arc<dyn TrustedSourceFactory>,
    pub clock: Arc<dyn SourceApplyClock>,
    pub limits: isolated_workspace::Limits,
    pub approval_ttl_ms: u64,
}

/// Explicit trusted setup only, never wire paths or the prepared copy's policy.
/// Runs on the blocking owner after actual completion. Return a pre-private,
/// synced and pinned per-operation recovery parent. No scan/apply/provider here.
/// The returned policy is fixed for the operation; persisted policy generations
/// remain the gate. External mutable factory state is not a later grant.
pub trait TrustedSourceFactory: Send + Sync {
    fn prepare(
        &self,
        completion: &real::TrustedRunCompletion,
        published: &Published,
        request: &SourceApplyRequest,
    ) -> Result<(CurrentSourcePolicy, PreparedSourceRecovery), ServiceError>;
}

struct Entry {
    target: RunTarget,
    receipt: Option<real::TrustedRunCompletion>,
    controller: Option<SourceApplyController>,
    // A failed spawn retains unconsumed recovery/work; never retry it implicitly.
    unstarted: Option<Work>,
    reserved: bool,
    unknown: bool,
    cancelled: bool,
    settled: bool,
    collection_revision: Option<DecimalU64>,
    // One explicit recovery identity per retained operation; never a growing log.
    recovery_request: Option<(RequestId, String)>,
    recovery_proof: Option<polaris_desktop_protocol::source_recovery::SavedResult>,
}
#[derive(Clone, Copy)]
enum Phase {
    Collect,
    Apply,
}
struct Running {
    index: usize,
    phase: Phase,
    job: Job,
}

pub(super) struct SourceOwner {
    runtime: SourceApplyRuntime,
    entries: Vec<Entry>,
    job: Option<Running>,
    #[cfg(test)]
    disconnect_after_answer: bool,
}
impl SourceOwner {
    pub(super) fn new(runtime: SourceApplyRuntime) -> Result<Self, ServiceError> {
        if runtime.approval_ttl_ms == 0 {
            return Err(ServiceError::Options);
        }
        Ok(Self {
            runtime,
            entries: Vec::new(),
            job: None,
            #[cfg(test)]
            disconnect_after_answer: false,
        })
    }
    pub(super) fn reserve(&mut self, target: RunTarget) -> Result<(), ProtocolError> {
        if self.entries.len() >= RUNS {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        self.entries.push(Entry {
            target,
            receipt: None,
            controller: None,
            unstarted: None,
            reserved: true,
            unknown: false,
            cancelled: false,
            settled: false,
            collection_revision: None,
            recovery_request: None,
            recovery_proof: None,
        });
        Ok(())
    }
    pub(super) fn release_unused(&mut self, target: &RunTarget) {
        self.entries
            .retain(|e| !(e.reserved && &e.target == target));
    }
    pub(super) fn completed(&mut self, receipt: real::TrustedRunCompletion) {
        // Reservation is created before accepting this run and remains until this
        // exact terminal handoff. Fill its existing slot; never enqueue/drop twice.
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.reserved && &e.target == receipt.target())
            .expect("accepted source run owns a reserved completion slot");
        entry.reserved = false;
        entry.receipt = Some(receipt);
    }
    #[cfg(test)]
    pub(super) fn disconnect_after_answer_for_test(&mut self) {
        self.disconnect_after_answer = true;
    }
    #[cfg(test)]
    pub(super) fn owned_for_test(&self) -> (usize, bool, usize) {
        (
            self.entries.len(),
            self.job.is_some(),
            self.entries
                .iter()
                .filter(|e| e.controller.as_ref().is_some_and(|c| c.report().is_some()))
                .count(),
        )
    }
    pub(super) fn unsettled(&self) -> bool {
        self.job.is_some() || self.entries.iter().any(|e| !e.settled)
    }
    pub(super) fn io_active(&self) -> bool {
        self.job.is_some()
    }
    pub(super) fn recovery_target(
        &self,
    ) -> Option<polaris_desktop_protocol::source_recovery::RetryTarget> {
        use polaris_desktop_protocol::source_recovery::{PayloadHash, RetryTarget};
        self.entries.iter().find_map(|entry| {
            let c = entry.controller.as_ref()?;
            if c.report().is_none()
                || !matches!(
                    c.state(),
                    SourceApplyState::ResultPendingSave | SourceApplyState::RecoveryRequired
                )
            {
                return None;
            }
            let candidate = c.candidate()?;
            Some(RetryTarget {
                run_id: entry.target.run_id.clone(),
                attempt_id: entry.target.attempt_id.clone(),
                approval_id: candidate.approval_id.clone(),
                operation_id: candidate.operation_id.clone(),
                payload_hash: PayloadHash::new(candidate.payload_hash.clone()).ok()?,
            })
        })
    }

    /// Result-only owner ingress. Verify every coordinate before touching recover.
    pub(super) fn retry_saved_result(
        &mut self,
        writer: &mut Writer,
        epoch: &EngineEpoch,
        request: &polaris_desktop_protocol::source_recovery::Request,
    ) -> Result<
        polaris_desktop_protocol::source_recovery::SavedResult,
        polaris_desktop_protocol::source_recovery::ErrorCode,
    > {
        use polaris_desktop_protocol::source_recovery::{
            ErrorCode as RecoveryError, Request as RecoveryRequest, SavedResult,
        };
        let RecoveryRequest::RetryResultSave {
            request_id,
            engine_epoch,
            project_id,
            session_id,
            run_id,
            attempt_id,
            approval_id,
            operation_id,
            payload_hash,
            ..
        } = request
        else {
            return Err(RecoveryError::TargetMismatch);
        };
        // Hash the bounded typed wire value, not an unbounded caller string.
        let bytes = polaris_desktop_protocol::source_recovery::encode(request)
            .map_err(|_| RecoveryError::TargetMismatch)?;
        let hash = polaris_core::conversation_state::content_hash(&bytes);
        if self.entries.iter().any(|entry| {
            entry
                .recovery_request
                .as_ref()
                .is_some_and(|(id, old)| id == request_id && old != &hash)
        }) {
            return Err(RecoveryError::RequestConflict);
        }
        if engine_epoch != epoch || writer.coordinates() != (project_id, session_id) {
            return Err(RecoveryError::TargetMismatch);
        }
        if self.job.is_some() {
            return Err(RecoveryError::Busy);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|e| &e.target.run_id == run_id && &e.target.attempt_id == attempt_id)
            .ok_or(RecoveryError::TargetMismatch)?;
        let controller = entry
            .controller
            .as_mut()
            .ok_or(RecoveryError::NoRetainedReport)?;
        let candidate = controller
            .candidate()
            .ok_or(RecoveryError::NoRetainedReport)?;
        if &candidate.approval_id != approval_id
            || &candidate.operation_id != operation_id
            || candidate.payload_hash != payload_hash.as_str()
            || controller.completion().project_id() != project_id
            || controller.completion().session_id() != session_id
        {
            return Err(RecoveryError::TargetMismatch);
        }
        if entry.unknown
            || controller.report().is_none()
            || !matches!(
                controller.state(),
                SourceApplyState::ResultPendingSave
                    | SourceApplyState::RecoveryRequired
                    | SourceApplyState::Saved
            )
        {
            return Err(RecoveryError::NoRetainedReport);
        }
        if entry
            .recovery_request
            .as_ref()
            .is_some_and(|(id, _)| id != request_id)
        {
            return Err(RecoveryError::RequestConflict);
        }
        entry.recovery_request = Some((request_id.clone(), hash));
        if let Some(proof) = &entry.recovery_proof {
            let saved = writer
                .snapshot()
                .map_err(|_| RecoveryError::StorageFailed)?;
            if !saved.state.source_applies.iter().any(|record| {
                record.candidate.operation_id == proof.operation_id
                    && record
                        .result
                        .as_ref()
                        .is_some_and(|result| result.result_id == proof.result_id)
            }) {
                return Err(RecoveryError::StorageFailed);
            }
            return Ok(proof.clone());
        }
        match controller.state() {
            SourceApplyState::RecoveryRequired => controller
                .retry_result_save(writer)
                .map_err(|_| RecoveryError::StorageFailed)?,
            SourceApplyState::ResultPendingSave => {
                // The engine may have failed before handing the joined report to
                // save_result. Explicit recovery is permitted only after the gates above.
                writer.recover().map_err(|_| RecoveryError::StorageFailed)?;
                let revision = writer
                    .snapshot()
                    .map_err(|_| RecoveryError::StorageFailed)?
                    .marker
                    .session_revision;
                controller
                    .save_result(writer, revision)
                    .map_err(|_| RecoveryError::StorageFailed)?;
            }
            SourceApplyState::Saved => {}
            _ => unreachable!("checked state"),
        }
        let saved = writer
            .snapshot()
            .map_err(|_| RecoveryError::StorageFailed)?;
        let result = saved
            .state
            .source_applies
            .iter()
            .find(|e| &e.candidate.operation_id == operation_id)
            .and_then(|e| e.result.as_ref())
            .ok_or(RecoveryError::StorageFailed)?;
        entry.settled = true;
        let proof = SavedResult {
            operation_id: operation_id.clone(),
            result_id: result.result_id.clone(),
            saved_revision: saved.marker.session_revision,
        };
        entry.recovery_proof = Some(proof.clone());
        Ok(proof)
    }
    pub(super) fn cancel_target(&mut self, target: &RunTarget) {
        for entry in &mut self.entries {
            if &entry.target == target {
                entry.cancelled = true;
            }
        }
    }
    fn start_job(
        &mut self,
        index: usize,
        phase: Phase,
        controller: SourceApplyController,
        work: Work,
    ) -> Result<(), ServiceError> {
        match source_apply_io::start(controller, work) {
            Ok(job) => self.job = Some(Running { index, phase, job }),
            Err((controller, error)) => {
                let entry = &mut self.entries[index];
                entry.controller = Some(controller);
                entry.unstarted = Some(error.work);
                // Precommit failed collection is safe to abandon, not to retry.
                // Postcommit spawn failure remains unknown and blocks readiness.
                entry.cancelled = true;
                entry.settled = matches!(phase, Phase::Collect);
                return Err(error.error.into());
            }
        }
        Ok(())
    }
    pub(super) fn poll(
        &mut self,
        writer: &mut Writer,
        stopping: bool,
        storage_failed: bool,
        disconnected: &AtomicBool,
    ) -> Result<(), ServiceError> {
        if stopping || storage_failed || disconnected.load(Ordering::Acquire) {
            for entry in &mut self.entries {
                entry.cancelled = true;
            }
        }
        let finished = self.job.as_mut().and_then(|running| running.job.poll());
        if let Some(finished) = finished {
            let running = self.job.take().expect("polled job exists");
            let entry = &mut self.entries[running.index];
            entry.controller = Some(finished.controller);
            match finished.outcome {
                Outcome::PanicUnknown => {
                    entry.cancelled = true;
                    // Preserve uncertain ownership indefinitely for explicit recovery.
                    entry.settled = false;
                    entry.unknown = true;
                }
                Outcome::Failed(error) => {
                    entry.cancelled = true;
                    entry.settled = matches!(running.phase, Phase::Collect);
                    if matches!(running.phase, Phase::Apply) {
                        return Err(source_service_error(error));
                    }
                }
                Outcome::Completed if !storage_failed => {
                    let controller = entry
                        .controller
                        .as_mut()
                        .expect("returned controller retained");
                    if matches!(running.phase, Phase::Collect) {
                        let saved = writer.snapshot()?;
                        if entry.cancelled
                            || disconnected.load(Ordering::Acquire)
                            || controller
                                .candidate()
                                .is_some_and(|c| c.payload.entries.is_empty())
                        {
                            entry.cancelled = true;
                            controller
                                .cancel(writer, saved.marker.session_revision)
                                .map_err(source_service_error)?;
                            entry.settled = true;
                        } else {
                            let revision = entry
                                .collection_revision
                                .expect("collect generation captured");
                            match controller.publish(writer, revision) {
                                Ok(_) => {}
                                Err(SourceApplyError::Store(e)) => return Err(e.into()),
                                Err(_) => {
                                    entry.cancelled = true;
                                    controller
                                        .cancel(writer, saved.marker.session_revision)
                                        .map_err(source_service_error)?;
                                    entry.settled = true;
                                }
                            }
                        }
                    } else {
                        let revision = writer.snapshot()?.marker.session_revision;
                        controller
                            .save_result(writer, revision)
                            .map_err(source_service_error)?;
                        entry.settled = true;
                    }
                }
                Outcome::Completed => {} // Failed storage: retain controller/report, no writes.
            }
        }
        if storage_failed {
            return Ok(());
        }
        for entry in &mut self.entries {
            if entry.settled || entry.unknown || entry.unstarted.is_some() {
                continue;
            }
            if let Some(controller) = &mut entry.controller {
                let precommit = matches!(
                    controller.state(),
                    SourceApplyState::Unprepared
                        | SourceApplyState::Collected
                        | SourceApplyState::Pending
                        | SourceApplyState::Allowed
                        | SourceApplyState::Invalidated
                );
                let saved = writer.snapshot()?;
                let expired = controller.candidate().is_some_and(|c| {
                    self.runtime.clock.now_ms() >= c.expires_at_unix_ms.get()
                        || saved.state.policy_revision != c.policy_revision
                });
                if precommit && (entry.cancelled || expired) {
                    controller
                        .cancel(writer, saved.marker.session_revision)
                        .map_err(source_service_error)?;
                    entry.cancelled = true;
                    entry.settled = true;
                }
            } else if entry.receipt.is_some() && entry.cancelled {
                entry.settled = true;
            }
        }
        if stopping || disconnected.load(Ordering::Acquire) || self.job.is_some() {
            return Ok(());
        }
        let Some(index) = self
            .entries
            .iter()
            .position(|e| e.receipt.is_some() && !e.cancelled && !e.settled)
        else {
            return Ok(());
        };
        let saved = writer.snapshot()?;
        let receipt = self.entries[index]
            .receipt
            .as_ref()
            .expect("receipt queued");
        let operation =
            operation_name(receipt.project_id(), receipt.session_id(), receipt.target());
        let request = SourceApplyRequest {
            approval_id: ApprovalId::new(operation.clone()).map_err(|_| ServiceError::Options)?,
            operation_id: OperationId::new(operation.clone()).map_err(|_| ServiceError::Options)?,
            result_id: ResultId::new(operation).map_err(|_| ServiceError::Options)?,
            expected_session_revision: saved.marker.session_revision,
            expires_at_unix_ms: DecimalU64::new(
                self.runtime
                    .clock
                    .now_ms()
                    .checked_add(self.runtime.approval_ttl_ms)
                    .ok_or(StoreError::Overflow)?,
            ),
        };
        let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runtime.factory.prepare(receipt, &saved, &request)
        }));
        let (policy, recovery) = match prepared {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => {
                self.entries[index].settled = true;
                self.entries[index].cancelled = true;
                return Ok(());
            }
            Err(payload) => {
                std::mem::forget(payload);
                self.entries[index].cancelled = true;
                self.entries[index].unknown = true;
                return Ok(());
            }
        };
        let policy_matches = policy.policy_revision == saved.state.policy_revision;
        let entry = &mut self.entries[index];
        entry.collection_revision = Some(request.expected_session_revision);
        let receipt = entry
            .receipt
            .take()
            .expect("receipt retained until setup succeeds");
        let controller = SourceApplyController::new(
            receipt,
            policy,
            self.runtime.clock.clone(),
            self.runtime.limits,
        );
        if disconnected.load(Ordering::Acquire) || !policy_matches {
            let entry = &mut self.entries[index];
            entry.controller = Some(controller);
            entry.unstarted = Some(Work::Collect { request, recovery });
            entry.cancelled = true;
            entry.settled = true;
        } else {
            self.start_job(
                index,
                Phase::Collect,
                controller,
                Work::Collect { request, recovery },
            )?;
        }
        Ok(())
    }

    pub(super) fn resolve(
        &mut self,
        writer: &mut Writer,
        request: &Request,
        draining: &bool,
        disconnected: &AtomicBool,
    ) -> Result<SuccessResult, ProtocolError> {
        let RequestBody::SourceApplyResolve(session, params) = &request.body else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        let known = writer
            .request_status(session, &request.client_id, &request.request_id)
            .map_err(store_error)?
            .is_some();
        // Replays always use exact content verification, even after restart or
        // while a controller is loaned to I/O. A saved ACK never grants dispatch.
        if known {
            writer
                .resolve_source_apply_request(request, self.runtime.clock.now_ms())
                .map_err(store_error)?;
            return Ok(source_ack(params));
        }
        if *draining
            || disconnected.load(Ordering::Acquire)
            || (params.decision == ApprovalDecision::Allow && self.job.is_some())
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        if writer.snapshot().map_err(store_error)?.state.requests.len() >= REQUEST_RECORDS - 1 {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        let index = self
            .entries
            .iter()
            .position(|entry| {
                !entry.cancelled
                    && entry.controller.as_ref().is_some_and(|c| {
                        c.candidate()
                            .is_some_and(|candidate| candidate.approval_id == params.approval_id)
                    })
            })
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let controller = self.entries[index]
            .controller
            .as_mut()
            .expect("matched controller");
        let receipt = controller
            .resolve_request(writer, request)
            .map_err(source_protocol_error)?;
        if receipt.newly_recorded && params.decision == ApprovalDecision::Allow {
            let revision = writer
                .snapshot()
                .map_err(store_error)?
                .marker
                .session_revision;
            #[cfg(test)]
            if std::mem::take(&mut self.disconnect_after_answer) {
                disconnected.store(true, Ordering::Release);
            }
            // Answer persistence can take time. Re-read the live disconnect gate
            // before consuming permission. Draining is serialized by this owner.
            if *draining || disconnected.load(Ordering::Acquire) {
                controller
                    .cancel(writer, revision)
                    .map_err(source_protocol_error)?;
                self.entries[index].cancelled = true;
                self.entries[index].settled = true;
                return Ok(source_ack(params));
            }
            let intent = controller
                .commit_intent(writer, revision, &params.payload_hash)
                .map_err(source_protocol_error)?;
            if intent == IntentReceipt::NewlyPublished {
                let controller = self.entries[index].controller.take().expect("intent owner");
                self.start_job(index, Phase::Apply, controller, Work::Apply)
                    .map_err(|_| error(ErrorCode::RecoveryRequired))?;
            }
        } else if params.decision == ApprovalDecision::Deny {
            self.entries[index].settled = true;
        }
        Ok(source_ack(params))
    }
}
fn source_ack(params: &polaris_desktop_protocol::request::SourceApplyResolve) -> SuccessResult {
    SuccessResult::SourceApplyResolve(ApprovalResolved {
        approval_id: params.approval_id.clone(),
        state: Resolved::Resolved,
        decision: params.decision,
    })
}
fn source_protocol_error(e: SourceApplyError) -> ProtocolError {
    match e {
        SourceApplyError::Store(e) => store_error(e),
        SourceApplyError::RecoveryRequired => error(ErrorCode::RecoveryRequired),
        SourceApplyError::Authority => error(ErrorCode::RevisionConflict),
        _ => error(ErrorCode::CapabilityUnavailable),
    }
}
fn source_service_error(e: SourceApplyError) -> ServiceError {
    match e {
        SourceApplyError::Store(e) => e.into(),
        _ => StoreError::RecoveryRequired.into(),
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use polaris_core::{
        desktop_execution::SandboxMode, desktop_store::SourceApplyIdentity,
        isolated_run::PreparedWorkspace, workspace_apply::RecoveryParent,
    };
    use polaris_desktop_protocol::{request::RunStart, source_recovery as wire};
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    struct Clock;
    impl SourceApplyClock for Clock {
        fn now_ms(&self) -> u64 {
            10
        }
    }
    struct Factory;
    impl TrustedSourceFactory for Factory {
        fn prepare(
            &self,
            _: &real::TrustedRunCompletion,
            _: &Published,
            _: &SourceApplyRequest,
        ) -> Result<(CurrentSourcePolicy, PreparedSourceRecovery), ServiceError> {
            panic!("recovery must not collect");
        }
    }
    fn fixture() -> (tempfile::TempDir, DesktopService, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ordinary"), "before").unwrap();
        let helper = base.join("helper");
        fs::write(&helper, "never launched").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let limits = isolated_workspace::Limits {
            max_entries: 16,
            max_files: 8,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 4,
        };
        let prepared = Arc::new(
            PreparedWorkspace::prepare(&source, &helper, SandboxMode::WorkspaceWrite, &[], limits)
                .unwrap(),
        );
        let root = PrototypeRoot::new().unwrap();
        let store = root
            .create(
                project_id(),
                session_id(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: "input".into(),
                        attachment_ids: vec![],
                    },
                    configuration: Configuration {
                        history_mode: Default::default(),
                        configuration_revision: DecimalU64::new(0),
                        provider: "fake".into(),
                        model: "offline".into(),
                        effort: "none".into(),
                    },
                    policy_revision: DecimalU64::new(0),
                },
            )
            .unwrap();
        let mut e = Engine::from_store(store, Options::default(), session_id());
        let target = RunTarget {
            run_id: RunId::new("run").unwrap(),
            attempt_id: AttemptId::new("attempt").unwrap(),
        };
        e.store
            .apply(
                &Request {
                    protocol_version: Default::default(),
                    client_id: ClientId::new("client").unwrap(),
                    request_id: RequestId::new("start").unwrap(),
                    body: RequestBody::RunStart(
                        e.session.clone(),
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
        let operation = OperationId::new("run-operation").unwrap();
        e.store.record_intent(&target, operation.clone()).unwrap();
        e.store
            .finish_with_messages(
                &target,
                &operation,
                Observation::Succeeded,
                ResultId::new("run-result").unwrap(),
                vec![polaris_provider::Message::assistant("answer")],
            )
            .unwrap();
        let saved = e.store.snapshot().unwrap();
        let identity = prepared.snapshot().source_identity;
        fs::write(prepared.snapshot().path().join("ordinary"), "after").unwrap();
        let receipt = real::TrustedRunCompletion::for_source_apply_test(prepared, &saved);
        let mut c = SourceApplyController::new(
            receipt,
            CurrentSourcePolicy {
                source_path: source.clone(),
                source_identity: SourceApplyIdentity {
                    device: DecimalU64::new(identity.device),
                    inode: DecimalU64::new(identity.inode),
                },
                policy_revision: saved.state.policy_revision,
                read_allowed: true,
                write_allowed: true,
            },
            Arc::new(Clock),
            limits,
        );
        let path = base.join("recovery");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let fd = fs::File::open(&path).unwrap();
        fd.sync_all().unwrap();
        fs::File::open(&base).unwrap().sync_all().unwrap();
        let m = fd.metadata().unwrap();
        let parent = RecoveryParent::pin(&fd, (m.dev(), m.ino()), &path).unwrap();
        c.prepare(
            &mut e.store,
            SourceApplyRequest {
                approval_id: ApprovalId::new("approval").unwrap(),
                operation_id: OperationId::new("apply").unwrap(),
                result_id: ResultId::new("result").unwrap(),
                expected_session_revision: saved.marker.session_revision,
                expires_at_unix_ms: DecimalU64::new(100),
            },
            PreparedSourceRecovery { parent, path },
        )
        .unwrap();
        let revision = e.store.snapshot().unwrap().marker.session_revision;
        c.resolve(&mut e.store, ApprovalDecision::Allow, revision)
            .unwrap();
        let revision = e.store.snapshot().unwrap().marker.session_revision;
        let hash = c.candidate().unwrap().payload_hash.clone();
        c.commit_intent(&mut e.store, revision, &hash).unwrap();
        c.apply().unwrap();
        let mut owner = SourceOwner::new(SourceApplyRuntime {
            factory: Arc::new(Factory),
            clock: Arc::new(Clock),
            limits,
            approval_ttl_ms: 100,
        })
        .unwrap();
        owner.reserve(target).unwrap();
        owner.entries[0].reserved = false;
        owner.entries[0].controller = Some(c);
        e.source = Some(owner);
        e.failed = true;
        e.production = true;
        e.draining = true;
        e.disconnected.store(true, Ordering::Release);
        (temp, DesktopService { engine: Some(e) }, source)
    }
    fn request(e: &Engine) -> wire::Request {
        let t = e.source.as_ref().unwrap().recovery_target().unwrap();
        let (project, session) = e.store.coordinates();
        wire::Request::RetryResultSave {
            version: Default::default(),
            request_id: RequestId::new("retry").unwrap(),
            engine_epoch: e.epoch.clone(),
            project_id: project.clone(),
            session_id: session.clone(),
            run_id: t.run_id,
            attempt_id: t.attempt_id,
            approval_id: t.approval_id,
            operation_id: t.operation_id,
            payload_hash: t.payload_hash,
        }
    }
    #[test]
    fn source_recovery_lost_ack_is_saved_once_and_conflicts_are_bounded() {
        let (_temp, mut service, source) = fixture();
        let handle = service.attach_source_recovery().unwrap();
        assert!(service.attach_source_recovery().is_err());
        let e = service.engine.as_mut().unwrap();
        let before = e.store.snapshot().unwrap();
        let req = request(e);
        let hello = wire::Request::Hello {
            version: Default::default(),
            request_id: RequestId::new("hello").unwrap(),
        };
        let mut answer = handle.try_request(hello.clone()).unwrap();
        assert!(handle.try_request(req.clone()).is_err());
        e.poll_source_recovery();
        let status = answer.try_recv().unwrap();
        status.validate_for_request(&hello).unwrap();
        assert_eq!(status.state, wire::State::ReportPending);
        let lost = handle.try_request(req.clone()).unwrap();
        drop(lost);
        e.poll_source_recovery();
        let saved = e.store.snapshot().unwrap();
        assert!(saved.state.source_applies[0].result.is_some());
        assert_eq!(before.state.runs, saved.state.runs);
        assert_eq!(before.state.configuration, saved.state.configuration);
        fs::write(source.join("ordinary"), "later editor").unwrap();
        let mut answer = handle.try_request(req.clone()).unwrap();
        e.poll_source_recovery();
        let proof = answer.try_recv().unwrap();
        proof.validate_for_request(&req).unwrap();
        assert_eq!(proof.state, wire::State::ReadyToExit);
        let mut conflicting = req.clone();
        if let wire::Request::RetryResultSave { payload_hash, .. } = &mut conflicting {
            *payload_hash = wire::PayloadHash::new("a".repeat(64)).unwrap();
        }
        let mut answer = handle.try_request(conflicting).unwrap();
        e.poll_source_recovery();
        let failure = answer.try_recv().unwrap();
        assert_eq!(failure.error, Some(wire::ErrorCode::RequestConflict));
        assert_ne!(failure.state, wire::State::ReadyToExit);
        let mut answer = handle.try_request(req.clone()).unwrap();
        e.poll_source_recovery();
        assert_eq!(answer.try_recv().unwrap().result, proof.result);
        assert_eq!(
            fs::read_to_string(source.join("ordinary")).unwrap(),
            "later editor"
        );
        assert_eq!(saved.marker, e.store.snapshot().unwrap().marker);
        e.settle_without_output().unwrap();
        assert!(e.ready);
    }
    #[test]
    fn source_recovery_wrong_target_before_recover_and_recovery_required_save() {
        let (_temp, mut service, _) = fixture();
        let handle = service.attach_source_recovery().unwrap();
        let e = service.engine.as_mut().unwrap();
        let req = request(e);
        let before = e.store.snapshot().unwrap();
        // A revision rejection moves the controller to RecoveryRequired without
        // inventing a report; explicit ingress must reconcile that same report.
        let controller = e.source.as_mut().unwrap().entries[0]
            .controller
            .as_mut()
            .unwrap();
        assert!(
            controller
                .save_result(&mut e.store, DecimalU64::new(0))
                .is_err()
        );
        let mut wrong = req.clone();
        if let wire::Request::RetryResultSave { engine_epoch, .. } = &mut wrong {
            *engine_epoch = EngineEpoch::new("other").unwrap();
        }
        let mut response = handle.try_request(wrong).unwrap();
        e.poll_source_recovery();
        assert_eq!(
            response.try_recv().unwrap().error,
            Some(wire::ErrorCode::TargetMismatch)
        );
        assert_eq!(before.marker, e.store.snapshot().unwrap().marker);
        assert!(e.failed);
        let mut response = handle.try_request(req.clone()).unwrap();
        e.poll_source_recovery();
        let response = response.try_recv().unwrap();
        response.validate_for_request(&req).unwrap();
        assert!(response.result.is_some());
        assert!(!e.failed);
        assert_eq!(
            e.source.as_ref().unwrap().entries[0]
                .controller
                .as_ref()
                .unwrap()
                .state(),
            SourceApplyState::Saved
        );
    }

    #[tokio::test]
    async fn source_recovery_bridge_uses_main_timer_then_failed_settle_owner() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn response(peer: &mut tokio::io::DuplexStream) -> wire::Response {
            let length = peer.read_u32().await.unwrap() as usize;
            assert!(length <= wire::MAX_FRAME_BYTES);
            let mut frame = (length as u32).to_be_bytes().to_vec();
            frame.resize(length + 4, 0);
            peer.read_exact(&mut frame[4..]).await.unwrap();
            wire::finish(&frame).unwrap().unwrap().0
        }
        let (_temp, mut service, source) = fixture();
        let handle = service.attach_source_recovery().unwrap();
        let e = service.engine.as_mut().unwrap();
        e.draining = false;
        e.disconnected.store(false, Ordering::Release);
        // The actual normal Step must service hello before the failed gate
        // transfers this same engine into its output-free settlement loop.
        let before = e.store.snapshot().unwrap();
        let retry = request(e);
        let hello = wire::Request::Hello {
            version: Default::default(),
            request_id: RequestId::new("bridge-hello").unwrap(),
        };
        let (mut peer, stream) = tokio::io::duplex(16 * 1024);
        peer.write_all(&wire::encode(&hello).unwrap())
            .await
            .unwrap();
        let mut bridge = Box::pin(crate::recovery_transport::serve_source_recovery(
            stream, handle,
        ));
        // Poll through the already-buffered frame into the owner's response
        // wait before starting its normal timer; no independent test Writer.
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(bridge.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let bridge = tokio::spawn(bridge);
        let (_main_peer, main_stream) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(main_stream);
        let owner = tokio::spawn(async move {
            let outcome = service.serve(reader, writer).await;
            (service, outcome)
        });
        let status = tokio::time::timeout(Duration::from_secs(5), response(&mut peer))
            .await
            .unwrap();
        status.validate_for_request(&hello).unwrap();
        assert_eq!(status.state, wire::State::ReportPending);
        peer.write_all(&wire::encode(&retry).unwrap())
            .await
            .unwrap();
        let saved = tokio::time::timeout(Duration::from_secs(5), response(&mut peer))
            .await
            .unwrap();
        saved.validate_for_request(&retry).unwrap();
        assert_eq!(saved.state, wire::State::ReadyToExit);
        assert!(saved.result.is_some());
        peer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), bridge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let (service, outcome) = tokio::time::timeout(Duration::from_secs(5), owner)
            .await
            .unwrap()
            .unwrap();
        // The original main-channel storage error remains honestly reported.
        assert!(outcome.is_err());
        let e = service.engine.as_ref().unwrap();
        assert!(e.ready);
        assert!(e.source_recovery.is_none());
        let after = e.store.snapshot().unwrap();
        assert!(after.state.source_applies[0].result.is_some());
        assert_eq!(before.state.runs, after.state.runs);
        assert_eq!(before.state.configuration, after.state.configuration);
        assert_eq!(
            fs::read_to_string(source.join("ordinary")).unwrap(),
            "after"
        );
    }
    #[tokio::test]
    async fn source_recovery_stop_idle_does_not_accept_buffered_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_temp, mut service, _) = fixture();
        let handle = service.attach_source_recovery().unwrap();
        let e = service.engine.as_mut().unwrap();
        let before = e.store.snapshot().unwrap();
        let (mut peer, stream) = tokio::io::duplex(8192);
        peer.write_all(&wire::encode(&request(e)).unwrap())
            .await
            .unwrap();
        crate::recovery_transport::serve_source_recovery_draining(
            stream,
            handle,
            std::future::ready(()),
        )
        .await
        .unwrap();
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        e.poll_source_recovery();
        assert_eq!(e.store.snapshot().unwrap().marker, before.marker);
        assert!(
            e.source.as_ref().unwrap().entries[0]
                .recovery_request
                .is_none()
        );
    }
    #[tokio::test]
    async fn source_recovery_stop_during_saved_ack_flushes_once_then_stops_receiving() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_temp, mut service, source) = fixture();
        let handle = service.attach_source_recovery().unwrap();
        let e = service.engine.as_mut().unwrap();
        let req = request(e);
        let hello = wire::Request::Hello {
            version: Default::default(),
            request_id: RequestId::new("second").unwrap(),
        };
        // One byte of output capacity guarantees that no complete ACK can be
        // written before this test begins reading, after durable save and stop.
        let (peer, stream) = tokio::io::duplex(1);
        let (mut reader, mut writer) = tokio::io::split(peer);
        let first = wire::encode(&req).unwrap();
        let second = wire::encode(&hello).unwrap();
        let sender = tokio::spawn(async move {
            writer.write_all(&first).await.unwrap();
            writer.write_all(&second).await
        });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let bridge = tokio::spawn(crate::recovery_transport::serve_source_recovery_draining(
            stream,
            handle,
            async {
                let _ = stopped.await;
            },
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while e.source.as_ref().unwrap().entries[0]
            .recovery_proof
            .is_none()
        {
            e.poll_source_recovery();
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let saved = e.store.snapshot().unwrap();
        assert!(!bridge.is_finished());
        stop.send(()).unwrap();
        let read_ack = async {
            let length = reader.read_u32().await.unwrap() as usize;
            assert!(length <= wire::MAX_FRAME_BYTES);
            let mut frame = (length as u32).to_be_bytes().to_vec();
            frame.resize(length + 4, 0);
            reader.read_exact(&mut frame[4..]).await.unwrap();
            let ack: wire::Response = wire::finish(&frame).unwrap().unwrap().0;
            ack.validate_for_request(&req).unwrap();
            assert_eq!(ack.state, wire::State::ReadyToExit);
            assert!(ack.result.is_some());
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
        };
        tokio::time::timeout(Duration::from_secs(5), read_ack)
            .await
            .unwrap();
        bridge.await.unwrap().unwrap();
        assert!(sender.await.unwrap().is_err());
        e.poll_source_recovery();
        assert_eq!(e.store.snapshot().unwrap().marker, saved.marker);
        assert_eq!(
            fs::read_to_string(source.join("ordinary")).unwrap(),
            "after"
        );
    }
    #[tokio::test]
    async fn source_recovery_owner_end_releases_queued_response_without_saving() {
        let (_temp, mut service, _) = fixture();
        let handle = service.attach_source_recovery().unwrap();
        let e = service.engine.as_mut().unwrap();
        let before = e.store.snapshot().unwrap();
        let req = request(e);
        let reply = handle.try_request(req.clone()).unwrap();
        e.close_source_recovery();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), reply)
                .await
                .unwrap()
                .is_err()
        );
        assert!(handle.try_request(req).is_err());
        assert_eq!(e.store.snapshot().unwrap().marker, before.marker);
        assert_eq!(e.source.as_ref().unwrap().owned_for_test(), (1, false, 1));
    }
}

// Length-delimited raw UTF-8 components preserve opaque identity (no path or
// Unicode normalization) while bounding every generated protocol ID to 71 bytes.
fn operation_name(project: &ProjectId, session: &SessionId, target: &RunTarget) -> String {
    let mut bytes = b"polaris-source-operation-v1".to_vec();
    for part in [
        project.as_str(),
        session.as_str(),
        target.run_id.as_str(),
        target.attempt_id.as_str(),
    ] {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part.as_bytes());
    }
    format!(
        "source-{}",
        polaris_core::conversation_state::content_hash(&bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_ids_bound_maximum_utf8_ids_and_preserve_each_coordinate() {
        let project = ProjectId::new("p".repeat(128)).unwrap();
        let session = SessionId::new("日".repeat(42) + "ab").unwrap();
        for run in ["r".repeat(122), "r".repeat(128), "語".repeat(42) + "ab"] {
            let target = RunTarget {
                run_id: RunId::new(run).unwrap(),
                attempt_id: AttemptId::new("a".repeat(128)).unwrap(),
            };
            let name = operation_name(&project, &session, &target);
            assert_eq!(name.len(), 71);
            ApprovalId::new(name.clone()).unwrap();
            OperationId::new(name.clone()).unwrap();
            ResultId::new(name.clone()).unwrap();
            assert_eq!(name, operation_name(&project, &session, &target));
            assert_ne!(
                name,
                operation_name(&ProjectId::new("other").unwrap(), &session, &target)
            );
            assert_ne!(
                name,
                operation_name(&project, &SessionId::new("other").unwrap(), &target)
            );
            let mut changed = target.clone();
            changed.attempt_id = AttemptId::new("other").unwrap();
            assert_ne!(name, operation_name(&project, &session, &changed));
            changed = target.clone();
            changed.run_id = RunId::new("other").unwrap();
            assert_ne!(name, operation_name(&project, &session, &changed));
        }
        let target = |run| RunTarget {
            run_id: RunId::new(run).unwrap(),
            attempt_id: AttemptId::new("a").unwrap(),
        };
        assert_ne!(
            operation_name(&project, &session, &target("é")),
            operation_name(&project, &session, &target("e\u{301}"))
        );
        assert_ne!(
            operation_name(
                &ProjectId::new("ab").unwrap(),
                &SessionId::new("c").unwrap(),
                &target("r")
            ),
            operation_name(
                &ProjectId::new("a").unwrap(),
                &SessionId::new("bc").unwrap(),
                &target("r")
            )
        );
    }
}
