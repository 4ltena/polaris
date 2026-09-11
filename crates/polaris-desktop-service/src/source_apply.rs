//! Trusted, single-operation source-apply owner. No RPC or model authorization.
//!
//! The caller serializes these methods and the borrowed Writer in the same owner.
//! Retain this controller on every ambiguous save; never drop its receipt/copy or
//! recovery FDs merely because a request/future disappeared. Recovery is explicit.
use crate::TrustedRunCompletion;
use polaris_core::{
    desktop_store::{
        Acceptance, IntentReceipt, SourceApplyCandidate, SourceApplyEntry, SourceApplyGuard,
        SourceApplyIdentity, SourceApplyIdentityProof, SourceApplyPayload, SourceApplyResult,
        SourceApplyVersion, StoreError, Writer,
    },
    isolated_workspace::{ChangeSet, Limits, ManifestEntry, protected_collect_changes},
    workspace_apply::{ApplyReport, RecoveryParent, protected_apply_pinned},
};
use polaris_desktop_protocol::{
    ids::*,
    request::{Request, RequestBody},
    snapshot::ApprovalDecision,
};
use std::{path::PathBuf, sync::Arc};

/// Current project-wide SOURCE capability, never the prepared copy's policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentSourcePolicy {
    pub source_path: PathBuf,
    pub source_identity: SourceApplyIdentity,
    pub policy_revision: DecimalU64,
    pub read_allowed: bool,
    pub write_allowed: bool,
}

/// The trusted controller samples time at each permission boundary.
pub trait SourceApplyClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceApplyState {
    Unprepared,
    Collected,
    Pending,
    Allowed,
    Invalidated,
    IntentCommitted,
    Dispatched,
    ResultPendingSave,
    Saved,
    RecoveryRequired,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceApplyError {
    #[error("source apply state does not permit this operation")]
    State,
    #[error("source apply authority, identity, target or generation mismatch")]
    Authority,
    #[error("source apply candidate cannot be represented or applied")]
    Candidate,
    #[error("source apply recovery directory validation failed")]
    RecoveryDirectory,
    #[error("source apply requires explicit recovery; no automatic dispatch")]
    RecoveryRequired,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
type Result<T> = std::result::Result<T, SourceApplyError>;

/// Trusted caller must create an operation-private directory outside source/copy,
/// on the same filesystem, fsync it AND its naming parent before handing it over.
/// `path` is the original provenance supplied to RecoveryParent::pin, not a path
/// to reopen. This object retains the FD even if that provenance is later renamed.
pub struct PreparedSourceRecovery {
    pub parent: RecoveryParent,
    pub path: PathBuf,
}

/// A request receipt is not dispatch authority. Only a fresh receipt may lead
/// the owner to commit_intent, and only IntentReceipt::NewlyPublished may dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDecisionReceipt {
    pub acceptance: Acceptance,
    pub newly_recorded: bool,
}

pub struct SourceApplyRequest {
    pub approval_id: ApprovalId,
    pub operation_id: OperationId,
    pub result_id: ResultId,
    pub expected_session_revision: DecimalU64,
    pub expires_at_unix_ms: DecimalU64,
}

#[must_use = "the single owner must retain copy/report/recovery FDs until explicit handoff"]
pub struct SourceApplyController {
    completion: TrustedRunCompletion,
    policy: CurrentSourcePolicy,
    clock: Arc<dyn SourceApplyClock>,
    limits: Limits,
    state: SourceApplyState,
    cancelled: bool,
    published: bool,
    collection_revision: Option<DecimalU64>,
    recovery: Option<PreparedSourceRecovery>,
    changes: Option<ChangeSet>,
    candidate: Option<SourceApplyCandidate>,
    result_id: Option<ResultId>,
    report: Option<ApplyReport>,
}

impl SourceApplyController {
    /// Store coordinates are derived only from the private completion receipt.
    /// Construction does not authorize, inspect or modify the source.
    pub fn new(
        completion: TrustedRunCompletion,
        policy: CurrentSourcePolicy,
        clock: Arc<dyn SourceApplyClock>,
        limits: Limits,
    ) -> Self {
        Self {
            completion,
            policy,
            clock,
            limits,
            state: SourceApplyState::Unprepared,
            cancelled: false,
            published: false,
            collection_revision: None,
            recovery: None,
            changes: None,
            candidate: None,
            result_id: None,
            report: None,
        }
    }

    pub fn state(&self) -> SourceApplyState {
        self.state
    }
    pub fn candidate(&self) -> Option<&SourceApplyCandidate> {
        self.candidate.as_ref()
    }
    pub fn report(&self) -> Option<&ApplyReport> {
        self.report.as_ref()
    }
    pub fn completion(&self) -> &TrustedRunCompletion {
        &self.completion
    }
    pub fn recovery_parent(&self) -> Option<&RecoveryParent> {
        self.recovery.as_ref().map(|r| &r.parent)
    }

    /// Publish only after protected collection, current source authority and the
    /// held recovery FD have all been checked. No mutable ChangeSet is exposed.
    pub fn prepare(
        &mut self,
        writer: &mut Writer,
        request: SourceApplyRequest,
        recovery: PreparedSourceRecovery,
    ) -> Result<&SourceApplyCandidate> {
        self.check_binding(writer)?;
        self.check_store(writer, request.expected_session_revision)?;
        let revision = request.expected_session_revision;
        self.collect(request, recovery)?;
        self.publish(writer, revision)
    }

    /// Writer-free bounded collection. Returning a candidate grants neither
    /// publication nor approval; the owner must call publish with fresh guards.
    pub fn collect(
        &mut self,
        request: SourceApplyRequest,
        recovery: PreparedSourceRecovery,
    ) -> Result<&SourceApplyCandidate> {
        if self.state != SourceApplyState::Unprepared || self.cancelled {
            return Err(SourceApplyError::State);
        }
        if recovery.path != recovery.parent.provenance() {
            return Err(SourceApplyError::RecoveryDirectory);
        }
        // Freeze the submitted generation before potentially expensive work.
        self.collection_revision = Some(request.expected_session_revision);
        // Retain supplied ownership even on validation/publication failure.
        self.recovery = Some(recovery);
        self.check_authority()?;
        let changes = protected_collect_changes(self.completion.prepared().snapshot(), self.limits)
            .map_err(|_| SourceApplyError::Candidate)?;
        if !changes.is_applicable() {
            return Err(SourceApplyError::Candidate);
        }
        let recovery = self.recovery.as_ref().ok_or(SourceApplyError::State)?;
        let identity = recovery.parent.identity();
        let mut entries = changes
            .changes
            .iter()
            .map(|change| {
                Ok(SourceApplyEntry {
                    relative_path: change
                        .relative_path
                        .to_str()
                        .ok_or(SourceApplyError::Candidate)?
                        .into(),
                    before: change
                        .before
                        .as_ref()
                        .map(|v| version(v, true))
                        .transpose()?,
                    after: change
                        .after
                        .as_ref()
                        .map(|v| version(v, false))
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        let payload = SourceApplyPayload {
            source_path: self
                .policy
                .source_path
                .to_str()
                .ok_or(SourceApplyError::Candidate)?
                .into(),
            source_identity: self.policy.source_identity,
            recovery_parent_path: recovery
                .path
                .to_str()
                .ok_or(SourceApplyError::Candidate)?
                .into(),
            recovery_parent_identity: source_identity(identity),
            entries,
        };
        let candidate = SourceApplyCandidate {
            run_id: self.completion.target().run_id.clone(),
            attempt_id: self.completion.target().attempt_id.clone(),
            approval_id: request.approval_id,
            operation_id: request.operation_id,
            policy_revision: self.policy.policy_revision,
            expires_at_unix_ms: request.expires_at_unix_ms,
            payload_hash: payload.payload_hash()?,
            payload: payload.clone(),
        };
        if report_upper_bound(&payload, &request.result_id)
            > polaris_core::desktop_store::MAX_SOURCE_APPLY_BYTES
        {
            return Err(SourceApplyError::Candidate);
        }
        self.changes = Some(changes);
        self.result_id = Some(request.result_id);
        self.candidate = Some(candidate);
        self.state = SourceApplyState::Collected;
        Ok(self.candidate.as_ref().expect("candidate retained"))
    }

    /// Owner-only publication after collection. Worker completion alone is not
    /// authority: frozen submitted generations, current Writer, source capability,
    /// FD and expiry are checked. The owner must process EOF/cancel via cancel
    /// before publishing a returned collection job.
    pub fn publish(
        &mut self,
        writer: &mut Writer,
        expected_session_revision: DecimalU64,
    ) -> Result<&SourceApplyCandidate> {
        self.check_binding(writer)?;
        if self.state != SourceApplyState::Collected || self.cancelled {
            return Err(SourceApplyError::State);
        }
        let candidate = self
            .candidate
            .as_ref()
            .ok_or(SourceApplyError::State)?
            .clone();
        if self.collection_revision != Some(expected_session_revision)
            || candidate.policy_revision != self.policy.policy_revision
        {
            return Err(SourceApplyError::Authority);
        }
        let proof = self.check_authority()?;
        let guard = self.guard(writer, expected_session_revision)?;
        let result = writer.publish_source_apply(self.completion.target(), candidate, guard, proof);
        self.saved(result)?;
        self.published = true;
        self.state = SourceApplyState::Pending;
        Ok(self.candidate.as_ref().expect("candidate retained"))
    }

    /// Atomically save a typed answer and its request receipt. Known requests
    /// return their receipt without changing local state, even after cancellation
    /// or dispatch; replay must never restart a job or consume another intent.
    pub fn resolve_request(
        &mut self,
        writer: &mut Writer,
        request: &Request,
    ) -> Result<SourceDecisionReceipt> {
        self.check_binding(writer)?;
        let RequestBody::SourceApplyResolve(session, params) = &request.body else {
            return Err(SourceApplyError::State);
        };
        let candidate = self.candidate.as_ref().ok_or(SourceApplyError::State)?;
        if session != self.completion.session_id()
            || params.run_id != candidate.run_id
            || params.attempt_id != candidate.attempt_id
            || params.approval_id != candidate.approval_id
            || params.payload_hash != candidate.payload_hash
            || params.expected_policy_revision != candidate.policy_revision
        {
            return Err(SourceApplyError::Authority);
        }
        let status = writer.request_status(session, &request.client_id, &request.request_id);
        let known = self.saved(status)?.is_some();
        if !known {
            if self.state != SourceApplyState::Pending || self.cancelled {
                return Err(SourceApplyError::State);
            }
            self.guard(writer, params.expected_session_revision)?;
        }
        // The writer verifies exact request content on replay; it must not be
        // replaced by directly returning request_status's receipt.
        let result = writer.resolve_source_apply_request(request, self.clock.now_ms());
        let acceptance = self.saved(result)?;
        if !known {
            self.state = if params.decision == ApprovalDecision::Allow {
                SourceApplyState::Allowed
            } else {
                SourceApplyState::Invalidated
            };
        }
        Ok(SourceDecisionReceipt {
            acceptance,
            newly_recorded: !known,
        })
    }

    pub fn resolve(
        &mut self,
        writer: &mut Writer,
        decision: ApprovalDecision,
        expected_session_revision: DecimalU64,
    ) -> Result<()> {
        self.check_binding(writer)?;
        if !matches!(
            self.state,
            SourceApplyState::Pending | SourceApplyState::Allowed
        ) || self.cancelled
        {
            return Err(SourceApplyError::State);
        }
        let guard = self.guard(writer, expected_session_revision)?;
        let candidate = self.candidate.as_ref().ok_or(SourceApplyError::State)?;
        let result = writer.resolve_source_apply(
            self.completion.target(),
            &candidate.approval_id,
            decision,
            guard,
        );
        self.saved(result)?;
        self.state = if decision == ApprovalDecision::Allow {
            SourceApplyState::Allowed
        } else {
            SourceApplyState::Invalidated
        };
        Ok(())
    }

    /// The successful durable commit is the permission linearization point.
    /// An ambiguous result or replay never creates a local dispatch capability.
    pub fn commit_intent(
        &mut self,
        writer: &mut Writer,
        expected_session_revision: DecimalU64,
        payload_hash: &str,
    ) -> Result<IntentReceipt> {
        self.check_binding(writer)?;
        if self.state != SourceApplyState::Allowed || self.cancelled {
            return Err(SourceApplyError::State);
        }
        let proof = self.check_authority()?;
        let guard = self.guard(writer, expected_session_revision)?;
        let candidate = self.candidate.as_ref().ok_or(SourceApplyError::State)?;
        if candidate.payload_hash != payload_hash {
            return Err(SourceApplyError::Candidate);
        }
        let result = writer.consume_source_apply_intent(
            self.completion.target(),
            &candidate.approval_id,
            payload_hash,
            guard,
            proof,
        );
        let receipt = self.saved(result)?;
        self.state = if receipt == IntentReceipt::NewlyPublished {
            SourceApplyState::IntentCommitted
        } else {
            SourceApplyState::RecoveryRequired
        };
        Ok(receipt)
    }

    /// Exactly one dispatch. Later cancellation/expiry/policy changes do not
    /// erase a committed operation or promise zero effects. Apply revalidates
    /// copy/source/auth registry and the pinned recovery scope itself.
    pub fn apply(&mut self) -> Result<&ApplyReport> {
        if self.state != SourceApplyState::IntentCommitted {
            return Err(SourceApplyError::State);
        }
        let changes = self.changes.as_ref().ok_or(SourceApplyError::State)?;
        let recovery = self.recovery.as_ref().ok_or(SourceApplyError::State)?;
        // Also blocks replay if an unexpected unwind interrupts the apply call.
        self.state = SourceApplyState::Dispatched;
        self.report = Some(protected_apply_pinned(
            self.completion.prepared().snapshot(),
            changes,
            self.limits,
            &recovery.parent,
        ));
        self.state = SourceApplyState::ResultPendingSave;
        Ok(self.report.as_ref().expect("report retained"))
    }

    /// Retain the original report and its live recovery FD even after saving.
    /// On failure only retry_result_save may reconcile; apply cannot be repeated.
    pub fn save_result(
        &mut self,
        writer: &mut Writer,
        expected_session_revision: DecimalU64,
    ) -> Result<()> {
        self.check_binding(writer)?;
        if !matches!(
            self.state,
            SourceApplyState::ResultPendingSave | SourceApplyState::Saved
        ) {
            return Err(SourceApplyError::State);
        }
        self.save_report(writer, expected_session_revision)
    }

    /// Explicit result-only reconciliation. Never converts a saved intent into
    /// permission, never reconstructs a report from source, never reruns apply.
    pub fn retry_result_save(&mut self, writer: &mut Writer) -> Result<()> {
        self.check_binding(writer)?;
        if self.state != SourceApplyState::RecoveryRequired || self.report.is_none() {
            return Err(SourceApplyError::State);
        }
        self.check_binding(writer)?;
        writer.recover()?;
        let revision = writer.snapshot()?.marker.session_revision;
        self.save_report(writer, revision)
    }

    /// Returns the actual stage, not a claim that committed effects were undone.
    pub fn cancel(
        &mut self,
        writer: &mut Writer,
        expected_session_revision: DecimalU64,
    ) -> Result<SourceApplyState> {
        self.check_binding(writer)?;
        self.cancelled = true;
        if matches!(
            self.state,
            SourceApplyState::Unprepared
                | SourceApplyState::Collected
                | SourceApplyState::Pending
                | SourceApplyState::Allowed
                | SourceApplyState::Invalidated
        ) {
            self.state = SourceApplyState::Invalidated;
            self.check_store(writer, expected_session_revision)?;
            if self.published {
                let candidate = self.candidate.as_ref().ok_or(SourceApplyError::State)?;
                let result = writer.invalidate_source_apply(
                    self.completion.target(),
                    &candidate.approval_id,
                    expected_session_revision,
                );
                self.saved(result)?;
            }
        }
        Ok(self.state)
    }

    /// Must be serialized with commit_intent by the same owner. Postcommit
    /// changes block future permissions but cannot revoke an already consumed one.
    pub fn update_policy(
        &mut self,
        writer: &mut Writer,
        policy: CurrentSourcePolicy,
        expected_session_revision: DecimalU64,
    ) -> Result<()> {
        self.check_store(writer, expected_session_revision)?;
        if policy == self.policy {
            return Ok(());
        }
        if policy.policy_revision <= self.policy.policy_revision {
            return Err(SourceApplyError::Authority);
        }
        self.policy = policy;
        if matches!(
            self.state,
            SourceApplyState::Unprepared
                | SourceApplyState::Collected
                | SourceApplyState::Pending
                | SourceApplyState::Allowed
        ) {
            self.cancelled = true;
            self.state = SourceApplyState::Invalidated;
        }
        let result = writer.set_policy_revision(self.policy.policy_revision);
        self.saved(result)
    }

    fn save_report(&mut self, writer: &mut Writer, revision: DecimalU64) -> Result<()> {
        // All failures preserve the report and prevent implicit retries.
        let result = (|| {
            self.check_store(writer, revision)?;
            let report = self.report.as_ref().ok_or(SourceApplyError::State)?;
            let candidate = self.candidate.as_ref().ok_or(SourceApplyError::State)?;
            writer.record_source_apply_result(
                self.completion.target(),
                &candidate.operation_id,
                revision,
                SourceApplyResult {
                    result_id: self.result_id.clone().ok_or(SourceApplyError::State)?,
                    report: serde_json::to_value(report)?,
                },
            )?;
            Ok(())
        })();
        if result.is_err() {
            self.state = SourceApplyState::RecoveryRequired;
        } else {
            self.state = SourceApplyState::Saved;
        }
        result
    }

    fn check_binding(&self, writer: &Writer) -> Result<()> {
        let (project, session) = writer.coordinates();
        if project != self.completion.project_id() || session != self.completion.session_id() {
            return Err(SourceApplyError::Authority);
        }
        Ok(())
    }
    fn check_store(&self, writer: &Writer, revision: DecimalU64) -> Result<()> {
        self.check_binding(writer)?;
        let saved = writer.snapshot()?;
        if saved.marker.session_revision != revision
            || revision < self.completion.session_revision()
            || !saved
                .state
                .runs
                .iter()
                .any(|r| r == self.completion.terminal())
        {
            return Err(SourceApplyError::Authority);
        }
        Ok(())
    }
    fn guard(&self, writer: &Writer, revision: DecimalU64) -> Result<SourceApplyGuard> {
        self.check_store(writer, revision)?;
        if writer.snapshot()?.state.policy_revision != self.policy.policy_revision {
            return Err(SourceApplyError::Authority);
        }
        Ok(SourceApplyGuard {
            expected_session_revision: revision,
            expected_policy_revision: self.policy.policy_revision,
            now_ms: self.clock.now_ms(),
        })
    }
    fn check_authority(&self) -> Result<SourceApplyIdentityProof> {
        let snapshot = self.completion.prepared().snapshot();
        let source = source_identity((
            snapshot.source_identity.device,
            snapshot.source_identity.inode,
        ));
        if !self.policy.read_allowed
            || !self.policy.write_allowed
            || self.policy.source_path != snapshot.source_path
            || self.policy.source_identity != source
        {
            return Err(SourceApplyError::Authority);
        }
        let recovery = self.recovery.as_ref().ok_or(SourceApplyError::State)?;
        recovery
            .parent
            .validate_for_snapshot(snapshot)
            .map_err(|_| SourceApplyError::RecoveryDirectory)?;
        Ok(SourceApplyIdentityProof {
            source,
            recovery_parent: source_identity(recovery.parent.identity()),
        })
    }
    fn saved<T>(&mut self, result: std::result::Result<T, StoreError>) -> Result<T> {
        result.map_err(|error| {
            self.state = SourceApplyState::RecoveryRequired;
            SourceApplyError::Store(error)
        })
    }
}
fn source_identity((device, inode): (u64, u64)) -> SourceApplyIdentity {
    SourceApplyIdentity {
        device: DecimalU64::new(device),
        inode: DecimalU64::new(inode),
    }
}
fn version(entry: &ManifestEntry, before: bool) -> Result<SourceApplyVersion> {
    let hash = entry.sha256.ok_or(SourceApplyError::Candidate)?;
    Ok(SourceApplyVersion {
        hash: hash.iter().map(|b| format!("{b:02x}")).collect(),
        mode: if before {
            entry.observed_mode()
        } else {
            entry.mode
        },
    })
}

// Upper bound for the current ApplyReport schema, including its result wrapper.
// JSON strings cost at most six bytes per UTF-8 byte. The 2048-byte fixed
// allowance per entry covers keys, phases, names (usize indices), all u64
// identities and flags; each ancestor gets another 128 bytes for tuple syntax
// and two maximal u64s. Top-level allowance includes failure fields and the
// generated 46-byte recovery child name. Saturation always fails closed.
fn report_upper_bound(payload: &SourceApplyPayload, result: &ResultId) -> usize {
    let string = |s: &str| s.len().saturating_mul(6).saturating_add(2);
    let mut bytes = 2048usize
        .saturating_add(string(result.as_str()))
        .saturating_add(string(&payload.source_path))
        .saturating_add(string(&payload.recovery_parent_path));
    for entry in &payload.entries {
        bytes = bytes
            .saturating_add(2048)
            .saturating_add(string(&entry.relative_path));
        let full = PathBuf::from(&payload.source_path).join(&entry.relative_path);
        for ancestor in full.parent().into_iter().flat_map(|p| p.ancestors()) {
            let Some(path) = ancestor.to_str() else {
                return usize::MAX;
            };
            bytes = bytes.saturating_add(128).saturating_add(string(path));
        }
    }
    bytes
}

#[cfg(test)]
mod tests;
