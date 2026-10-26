//! TempDirの所有、writerのOS lock、対象別CASと実行意図・終端の公開をまとめる。

use super::{disk, model::*, source_apply::*};
use crate::conversation_state::{RawEventV2, content_hash};
use crate::workflow::{
    ActiveEvidenceArtifactsV1, ConvergenceInputV1, ObservedEvidenceSubjectV1,
    build_owner_convergence, validate_owner_convergence,
};
#[cfg(all(test, windows))]
use polaris_auth::protection::WindowsFamilyFile as File;
use polaris_desktop_protocol::{
    ids::*,
    request::{Request, RequestBody, RunTarget},
    run_state::{Control, Observation, RunInput, RunState},
    snapshot::{ApprovalDecision, Draft, PendingApproval, Run},
};
use polaris_provider::Message;
#[cfg(not(all(test, windows)))]
use std::fs::File;
use std::{fs, io::Write, sync::Arc};
use tempfile::TempDir;

pub(super) mod details;
mod history;
mod validation;
use validation::validate_ledger;
pub(super) mod mcp;
pub(in crate::desktop_store) mod migration;
pub(super) mod prompt;
pub(super) mod recipe;
pub use details::{RunDetailEntry, RunDetails};
pub use history::OwnerRequestHistory;

/// 任意pathや既存TempDirを受け取る入口を持たない、試作領域の所有者。
#[derive(Debug, Clone)]
pub struct PrototypeRoot {
    backing: StoreRoot,
}

/// Durable owner of an existing private directory. Never deletes it on drop.
#[derive(Debug, Clone)]
pub struct DesktopRoot {
    backing: StoreRoot,
}
#[derive(Debug, Clone)]
struct StoreRoot {
    directory: Arc<disk::Directory>,
    // The prototype alone owns deletion; Writers retain this owner until released.
    _temporary: Option<Arc<TempDir>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopCapacityIdentityV1 {
    pub root_identity: crate::apply_intent::IdentityV1,
    pub allocation_unit: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopLogicalCapacityV1 {
    pub records_remaining: u64,
    pub bytes_remaining: u64,
}

/// A read-only prediction bound to the retained writer lock. It deliberately
/// cannot be cloned or constructed by callers; the shared owner consumes it once.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub struct SourceApplyPrecommitTokenV1 {
    precommit: SourceApplyPrecommitV1,
    next: Published,
    prediction: disk::CommitPredictionM2a,
    root_identity: DesktopCapacityIdentityV1,
    session_identity: (u64, u64),
    lock_identity: (u64, u64),
    logical_capacity: DesktopLogicalCapacityV1,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SourceApplyPrecommitTokenV1 {
    pub fn precommit(&self) -> &SourceApplyPrecommitV1 {
        &self.precommit
    }
    pub fn logical_capacity(&self) -> DesktopLogicalCapacityV1 {
        self.logical_capacity
    }

    /// Counts each new row with the actual v2 encoder. A result replaces only
    /// this record's null; both marker temporaries use the approved hard cap.
    pub fn inventory_commit_bytes(&self) -> StoreResult<u64> {
        let consume = u64::try_from(
            self.prediction.target_sidecar_prefix.len() - self.prediction.base_sidecar_prefix.len(),
        )
        .map_err(|_| StoreError::Overflow)?;
        let mut result_state = self.next.state.clone();
        result_state.session_revision = result_state.session_revision.checked_add(1)?;
        let result = encoded_size_m2a(&result_state)?
            .checked_add(1)
            .and_then(|n| n.checked_sub(4))
            .and_then(|n| n.checked_add(self.precommit.result_capacity_bytes))
            .ok_or(StoreError::Overflow)?;
        let unit = self.root_identity.allocation_unit;
        let allocated = |bytes: u64| -> StoreResult<u64> {
            bytes
                .checked_add(unit - 1)
                .and_then(|n| (n / unit).checked_mul(unit))
                .ok_or(StoreError::Overflow)
        };
        allocated(consume)?
            .checked_add(allocated(result)?)
            .and_then(|n| {
                n.checked_add(
                    allocated(MAX_D27_MARKER_BYTES as u64)
                        .ok()?
                        .checked_mul(2)?,
                )
            })
            .ok_or(StoreError::Overflow)
    }

    pub(crate) fn result_remaining_bytes(&self) -> StoreResult<u64> {
        let unit = self.root_identity.allocation_unit;
        let consume = publication_remaining_bytes_m2a(
            &self.prediction,
            &self.prediction.base_marker_bytes,
            &self.prediction.base_sidecar_prefix,
            0,
            unit,
        )?;
        self.inventory_commit_bytes()?
            .checked_sub(consume)
            .ok_or(StoreError::Overflow)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publication_remaining_bytes_m2a(
    prediction: &disk::CommitPredictionM2a,
    observed_marker: &[u8],
    observed_sidecar: &[u8],
    temporary_bytes: u64,
    unit: u64,
) -> StoreResult<u64> {
    let allocated =
        |bytes| crate::apply_intent::allocated_bytes(bytes, unit).map_err(|_| StoreError::Overflow);
    let sidecar = allocated(prediction.target_sidecar_prefix.len() as u64)?
        .checked_sub(allocated(observed_sidecar.len() as u64)?)
        .ok_or(StoreError::RecoveryRequired)?;
    let marker = if observed_marker == prediction.target_marker_bytes {
        0
    } else {
        allocated(MAX_D27_MARKER_BYTES as u64)?.saturating_sub(allocated(temporary_bytes)?)
    };
    sidecar.checked_add(marker).ok_or(StoreError::Overflow)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub struct SourceApplyConsumedReceiptV1 {
    pub precommit: SourceApplyPrecommitV1,
    pub marker_bytes: Vec<u8>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceApplyResultEvidenceV1 {
    pub session_revision: u64,
    pub marker_bytes: Vec<u8>,
    pub marker_evidence_hash: crate::management_effect::ManagementEffectHashV1,
    pub sidecar_offset: u64,
    pub sidecar_hash: crate::management_effect::ManagementEffectHashV1,
    pub sidecar_prefix_hash: crate::management_effect::ManagementEffectHashV1,
    pub reserved_result_id_digest: crate::management_effect::ManagementEffectHashV1,
    pub report_hash: crate::management_effect::ManagementEffectHashV1,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LegacyUnboundIntentDiagnosticV1 {
    pub schema_version: u32,
    pub code: &'static str,
    pub project_binding: crate::management_effect::ManagementEffectHashV1,
    pub session_binding: crate::management_effect::ManagementEffectHashV1,
    pub marker_evidence_hash: crate::management_effect::ManagementEffectHashV1,
    pub sidecar_prefix_hash: crate::management_effect::ManagementEffectHashV1,
    pub session_revision: u64,
    pub record_index: u64,
    pub intent_revision: u64,
    pub source_operation_digest: crate::management_effect::ManagementEffectHashV1,
    pub result_present: bool,
    pub next_action: &'static str,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, thiserror::Error)]
pub enum D27RecoveryOpenError {
    #[error("legacy source apply requires its original desktop recovery route")]
    LegacyUnboundIntent(Box<LegacyUnboundIntentDiagnosticV1>),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
pub struct D27RecoveryInspectV1 {
    pub root_identity: crate::apply_intent::IdentityV1,
    pub session_identity: crate::apply_intent::IdentityV1,
    pub writer_lock_identity: crate::apply_intent::IdentityV1,
    pub marker_bytes: Vec<u8>,
    pub marker_evidence_hash: crate::management_effect::ManagementEffectHashV1,
    pub raw_prefix_bytes: Vec<u8>,
    pub raw_tail_bytes: Vec<u8>,
    pub sidecar_prefix_bytes: Vec<u8>,
    pub sidecar_tail_bytes: Vec<u8>,
    pub snapshot: Published,
    pub state_next: disk::StateNextInspectV1,
    pub files: Vec<disk::D27ReadFileV1>,
}

/// Retains the exclusive existing writer lock without exposing mutating Writer
/// APIs. The shared owner must exact-join durable evidence before recovering.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub struct D27RecoveryOpenOutcomeV1 {
    pub view: D27RecoveryInspectV1,
    pub legacy_terminal_notices: Vec<LegacyUnboundIntentDiagnosticV1>,
    writer: Writer,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D27PublicationKindV1 {
    Consume,
    Result,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub struct JoinedD27MarkerRecoveryV1 {
    kind: D27PublicationKindV1,
    prediction: disk::CommitPredictionM2a,
    next: Published,
    observed_marker: Vec<u8>,
    observed_raw: Vec<u8>,
    observed_sidecar: Vec<u8>,
    observed_next: disk::StateNextInspectV1,
    root_identity: crate::apply_intent::IdentityV1,
    session_identity: crate::apply_intent::IdentityV1,
    lock_identity: crate::apply_intent::IdentityV1,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl JoinedD27MarkerRecoveryV1 {
    pub(crate) fn remaining_publication_bytes(&self, unit: u64) -> StoreResult<u64> {
        publication_remaining_bytes_m2a(
            &self.prediction,
            &self.observed_marker,
            &self.observed_sidecar,
            self.observed_next.length.unwrap_or(0),
            unit,
        )
    }
    pub(crate) fn result_evidence_m2a(
        &self,
        precommit: &SourceApplyPrecommitV1,
        report_hash: crate::management_effect::ManagementEffectHashV1,
    ) -> StoreResult<SourceApplyResultEvidenceV1> {
        use crate::management_effect::ManagementEffectHashV1 as Hash;
        if self.kind != D27PublicationKindV1::Result {
            return Err(StoreError::RecoveryRequired);
        }
        let marker = &self.prediction.target_marker;
        let bytes = &self.prediction.target_marker_bytes;
        let sidecar = &self.prediction.target_sidecar_prefix;
        Ok(SourceApplyResultEvidenceV1 {
            session_revision: marker.session_revision.get(),
            marker_bytes: bytes.clone(),
            marker_evidence_hash: marker_evidence_hash(bytes, sidecar),
            sidecar_offset: marker.sidecar_offset.get(),
            sidecar_hash: Hash::from_hex(&marker.sidecar_hash)
                .map_err(|_| StoreError::RecoveryRequired)?,
            sidecar_prefix_hash: Hash::digest(sidecar),
            reserved_result_id_digest: precommit.reserved_result_id_digest,
            report_hash,
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl D27RecoveryOpenOutcomeV1 {
    pub(crate) fn retained_root_directory_m2a(&self) -> StoreResult<File> {
        self.writer.root.directory.verify()?;
        self.writer.root.directory.retained_file_m2a()
    }
    /// Only the shared core owner calls this after joining the durable intent
    /// and verified generic event. Every ledger target is rebuilt here.
    pub(crate) fn join_marker_recovery_m2a(
        &self,
        precommit: &SourceApplyPrecommitV1,
        descriptor: &crate::apply_intent::ApplyIntentDescriptorV1,
        binding: &crate::apply_intent::D27SourceBindingV1,
        (request, before): (
            &crate::apply_intent::RequestV1,
            &crate::apply_intent::ObservedSourceV1,
        ),
        generic: &crate::management_effect::PrepareManagementEffectV1,
        report: Option<(&[u8], crate::management_effect::ManagementEffectHashV1)>,
    ) -> StoreResult<JoinedD27MarkerRecoveryV1> {
        use crate::{
            apply_intent::d27_session_binding, management_effect::ManagementEffectHashV1 as Hash,
        };
        use std::os::unix::fs::MetadataExt;
        precommit.canonical_bytes()?;
        binding
            .verify_artifacts(descriptor, request, before)
            .map_err(|_| StoreError::TargetMismatch)?;
        let expected_generic = crate::management_effect::PrepareManagementEffectV1 {
            operation_id: descriptor.management_operation_id,
            inventory: crate::management_effect::ManagementInventoryIdV1::parse("D27")
                .map_err(|_| StoreError::TargetMismatch)?,
            scope: crate::management_effect::ManagementEffectScopeV1 {
                project: descriptor.project_binding,
                session: Some(
                    d27_session_binding(self.writer.published.marker.session_id.as_str())
                        .map_err(|_| StoreError::TargetMismatch)?,
                ),
                generation: self.writer.published.marker.epoch.get(),
                revision: precommit.consume_target_revision,
                permission: crate::management_effect::ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: descriptor
                .descriptor_hash()
                .map_err(|_| StoreError::TargetMismatch)?,
            rewind: crate::management_effect::ManagementRewindDispositionV1::IrreversibleLocal,
        };
        if *generic != expected_generic {
            return Err(StoreError::TargetMismatch);
        }
        let observed = disk::inspect_published_m2a(
            &self.writer.directory,
            &self.writer.published.marker.project_id,
            &self.writer.published.marker.session_id,
        )?;
        let (base, marker, raw, sidecar) =
            disk::historical_published_m2a(&observed, precommit.base_session_revision)?;
        if marker_evidence_hash(&marker, &sidecar) != precommit.base_marker_evidence_hash {
            return Err(StoreError::RecoveryRequired);
        }
        let index = base
            .state
            .source_applies
            .iter()
            .position(|r| {
                r.candidate.run_id == precommit.run_id
                    && r.candidate.attempt_id == precommit.attempt_id
                    && r.candidate.approval_id == precommit.approval_id
            })
            .ok_or(StoreError::NotFound)?;
        let record = &base.state.source_applies[index];
        if record.decision != Some(ApprovalDecision::Allow)
            || record.invalidated
            || record.intent_revision.is_some()
            || record.reserved_result_id.is_some()
            || record.result.is_some()
            || record.candidate.payload_hash != binding.payload_hash.as_hex()
            || serde_json::to_vec(&record.candidate.payload)?.len() as u64 != binding.payload_bytes
        {
            return Err(StoreError::TargetMismatch);
        }
        let mut consumed = base.clone();
        consumed.state.source_applies[index].intent_revision =
            Some(DecimalU64::new(precommit.consume_target_revision));
        consumed.state.source_applies[index].reserved_result_id =
            Some(precommit.reserved_result_id.clone());
        remaining_ledger_bytes_m2a(&consumed.state)?;
        let mut consumed = prepare_transition_m2a(&base, consumed, false)?;
        self.writer.validate_d27_descriptor_m2a(
            precommit,
            descriptor,
            &consumed.state.source_applies[index],
            &precommit.reserved_result_id,
        )?;
        let mut record_bytes = b"polaris-d27-consume-record-v1\0".to_vec();
        record_bytes.extend(serde_json::to_vec(&consumed.state.source_applies[index])?);
        let consume_prediction =
            disk::predict_from_prefixes_m2a(&base, &consumed, marker, raw, sidecar)?;
        if Hash::digest(&record_bytes) != precommit.consume_record_hash
            || consume_prediction.target_marker.sidecar_offset.get()
                != precommit.consume_sidecar_offset
            || consume_prediction.target_marker.sidecar_hash
                != precommit.consume_sidecar_hash.as_hex()
            || marker_evidence_hash(
                &consume_prediction.target_marker_bytes,
                &consume_prediction.target_sidecar_prefix,
            ) != precommit.consume_marker_evidence_hash
        {
            return Err(StoreError::RecoveryRequired);
        }
        consumed.marker = consume_prediction.target_marker.clone();
        let (kind, prediction, next) = if let Some((bytes, expected_hash)) = report {
            if bytes.len() as u64 > precommit.result_capacity_bytes {
                return Err(StoreError::Capacity("D27 report artifact"));
            }
            let typed: crate::workspace_apply::ApplyReport = serde_json::from_slice(bytes)?;
            let mut hash_bytes = b"polaris-workspace-apply-report-v1\0".to_vec();
            hash_bytes.extend(bytes);
            if typed
                .canonical_bytes()
                .map_err(|_| StoreError::Corrupt("D27 report encoding"))?
                != bytes
                || Hash::digest(&hash_bytes) != expected_hash
                || typed.source_path != std::path::Path::new(&record.candidate.payload.source_path)
            {
                return Err(StoreError::TargetMismatch);
            }
            let result = SourceApplyResult {
                result_id: precommit.reserved_result_id.clone(),
                report: serde_json::to_value(typed)?,
            };
            result.validate()?;
            if serde_json::to_vec(&result)?.len() as u64 > precommit.result_capacity_bytes {
                return Err(StoreError::Capacity("D27 result evidence"));
            }
            // Policy changes after consume cannot revoke dispatch. Rebuild
            // result publication from the retained result-free generation,
            // while requiring the exact consumed row and immutable prefix.
            let current_record = observed
                .published
                .state
                .source_applies
                .get(index)
                .ok_or(StoreError::RecoveryRequired)?;
            let base_revision = if current_record.result.is_some() {
                observed
                    .published
                    .marker
                    .session_revision
                    .get()
                    .checked_sub(1)
                    .ok_or(StoreError::Overflow)?
            } else {
                observed.published.marker.session_revision.get()
            };
            if base_revision < precommit.consume_target_revision {
                return Err(StoreError::RecoveryRequired);
            }
            let (result_base, marker, raw, sidecar) =
                disk::historical_published_m2a(&observed, base_revision)?;
            if result_base.state.source_applies.get(index)
                != consumed.state.source_applies.get(index)
            {
                return Err(StoreError::RecoveryRequired);
            }
            let mut next = result_base.clone();
            next.state.source_applies[index].result = Some(result);
            let mut next = prepare_transition_m2a(&result_base, next, false)?;
            let prediction =
                disk::predict_from_prefixes_m2a(&result_base, &next, marker, raw, sidecar)?;
            require_d27_result_capacity_m2a(
                descriptor,
                consume_prediction.target_sidecar_prefix.len()
                    - consume_prediction.base_sidecar_prefix.len(),
                prediction.target_sidecar_prefix.len() - prediction.base_sidecar_prefix.len(),
            )?;
            next.marker = prediction.target_marker.clone();
            (D27PublicationKindV1::Result, prediction, next)
        } else {
            (D27PublicationKindV1::Consume, consume_prediction, consumed)
        };
        let state_next = self
            .writer
            .directory
            .inspect_next_m2a(Some(&prediction.target_marker_bytes))?;
        if !matches!(
            state_next.classification,
            disk::StateNextClassificationV1::Absent
                | disk::StateNextClassificationV1::ProperPrefix
                | disk::StateNextClassificationV1::ExactTarget
        ) {
            return Err(StoreError::RecoveryRequired);
        }
        let marker_is_base = observed.marker_bytes == prediction.base_marker_bytes;
        let marker_is_target = observed.marker_bytes == prediction.target_marker_bytes;
        let logs_are_base = observed.raw_bytes == prediction.base_raw_prefix
            && observed.sidecar_bytes == prediction.base_sidecar_prefix;
        let logs_are_target = observed.raw_bytes == prediction.target_raw_prefix
            && observed.sidecar_bytes == prediction.target_sidecar_prefix;
        if !(marker_is_base
            && logs_are_base
            && state_next.classification == disk::StateNextClassificationV1::Absent
            || marker_is_base && logs_are_target
            || marker_is_target
                && logs_are_target
                && state_next.classification == disk::StateNextClassificationV1::Absent)
        {
            return Err(StoreError::RecoveryRequired);
        }
        let (device, inode) = self.writer.root.directory.identity()?;
        let root_identity = crate::apply_intent::IdentityV1 { device, inode };
        let (device, inode) = self.writer.directory.identity()?;
        let session_identity = crate::apply_intent::IdentityV1 { device, inode };
        let lock = self.writer.guard.metadata()?;
        disk::same(
            &self.writer.guard,
            &self.writer.directory.open("writer.lock", true, false)?,
        )?;
        if lock.mode() & 0o777 != 0o600 {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(JoinedD27MarkerRecoveryV1 {
            kind,
            prediction,
            next,
            observed_marker: observed.marker_bytes,
            observed_raw: observed.raw_bytes,
            observed_sidecar: observed.sidecar_bytes,
            observed_next: state_next,
            root_identity,
            session_identity,
            lock_identity: crate::apply_intent::IdentityV1 {
                device: lock.dev(),
                inode: lock.ino(),
            },
        })
    }

    pub(crate) fn recover_published_m2a(
        mut self,
        token: JoinedD27MarkerRecoveryV1,
    ) -> StoreResult<(Writer, bool, D27PublicationKindV1)> {
        use std::os::unix::fs::MetadataExt;
        let (device, inode) = self.writer.root.directory.identity()?;
        if token.root_identity != (crate::apply_intent::IdentityV1 { device, inode }) {
            return Err(StoreError::RecoveryRequired);
        }
        let (device, inode) = self.writer.directory.identity()?;
        let lock = self.writer.guard.metadata()?;
        if token.session_identity != (crate::apply_intent::IdentityV1 { device, inode })
            || token.lock_identity
                != (crate::apply_intent::IdentityV1 {
                    device: lock.dev(),
                    inode: lock.ino(),
                })
            || lock.mode() & 0o777 != 0o600
        {
            return Err(StoreError::RecoveryRequired);
        }
        disk::same(
            &self.writer.guard,
            &self.writer.directory.open("writer.lock", true, false)?,
        )?;
        let committed = disk::recover_published_m2a(
            &self.writer.directory,
            &token.prediction,
            &token.observed_marker,
            &token.observed_raw,
            &token.observed_sidecar,
            &token.observed_next,
        )?;
        if committed {
            self.writer.published = token.next;
        }
        self.writer.recovery_required = false;
        Ok((self.writer, committed, token.kind))
    }
}

impl PrototypeRoot {
    pub fn new() -> StoreResult<Self> {
        disk::supported()?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("polaris-desktop-v3-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let temporary = Arc::new(builder.tempdir()?);
        let directory = disk::Directory::open_owned(&fs::canonicalize(temporary.path())?)?;
        Ok(Self {
            backing: StoreRoot {
                directory: Arc::new(directory),
                _temporary: Some(temporary),
            },
        })
    }
    pub fn create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        self.backing.create(project, session, initial)
    }
    pub fn open(&self, project: &ProjectId, session: &SessionId) -> StoreResult<Writer> {
        self.backing.open(project, session)
    }
}
impl DesktopRoot {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn recovery_open_m2a(
        &self,
        project: &ProjectId,
        session: &SessionId,
        project_binding: crate::management_effect::ManagementEffectHashV1,
    ) -> Result<D27RecoveryOpenOutcomeV1, D27RecoveryOpenError> {
        use crate::apply_intent::IdentityV1;
        use std::os::unix::fs::MetadataExt;
        self.backing.directory.verify()?;
        let directory = Arc::new(
            self.backing
                .directory
                .child(&content_hash(session.as_str().as_bytes()), false)?,
        );
        let guard = directory.open("writer.lock", true, false)?;
        acquire(&guard)?;
        disk::same(&guard, &directory.open("writer.lock", true, false)?)?;
        let inspected = disk::inspect_published_m2a(&directory, project, session)?;
        validate_ledger(&inspected.published)?;
        validate_artifact_bodies_with_mode(&self.backing, &inspected.published, true)?;
        let (device, inode) = self.identity()?;
        let root_identity = IdentityV1 { device, inode };
        let (device, inode) = directory.identity()?;
        let session_identity = IdentityV1 { device, inode };
        let lock = guard.metadata().map_err(StoreError::Io)?;
        let writer_lock_identity = IdentityV1 {
            device: lock.dev(),
            inode: lock.ino(),
        };
        let raw_offset = inspected.published.marker.raw_offset.get() as usize;
        let sidecar_offset = inspected.published.marker.sidecar_offset.get() as usize;
        let raw_prefix_bytes = inspected.raw_bytes[..raw_offset].to_vec();
        let sidecar_prefix_bytes = inspected.sidecar_bytes[..sidecar_offset].to_vec();
        let marker_evidence_hash =
            marker_evidence_hash(&inspected.marker_bytes, &sidecar_prefix_bytes);
        let legacy_terminal_notices = legacy_unbound_notices_m2a(
            &inspected.published,
            project_binding,
            marker_evidence_hash,
            &sidecar_prefix_bytes,
        )?;
        let view = D27RecoveryInspectV1 {
            root_identity,
            session_identity,
            writer_lock_identity,
            marker_bytes: inspected.marker_bytes,
            marker_evidence_hash,
            raw_prefix_bytes,
            raw_tail_bytes: inspected.raw_bytes[raw_offset..].to_vec(),
            sidecar_prefix_bytes,
            sidecar_tail_bytes: inspected.sidecar_bytes[sidecar_offset..].to_vec(),
            snapshot: inspected.published.clone(),
            state_next: inspected.state_next,
            files: inspected.files,
        };
        Ok(D27RecoveryOpenOutcomeV1 {
            view,
            legacy_terminal_notices,
            writer: Writer {
                root: self.backing.clone(),
                directory,
                guard,
                published: inspected.published,
                recovery_required: true,
            },
        })
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn capacity_identity_m2a(&self) -> StoreResult<DesktopCapacityIdentityV1> {
        self.backing.capacity_identity_m2a()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn available_bytes_m2a(&self) -> StoreResult<u64> {
        self.backing
            .directory
            .capacity_m2a()
            .map(|(_, available)| available)
    }
    /// Verify and identify the retained private directory without reopening it.
    /// Used to compare native publication coordinates before consuming a Writer.
    pub fn identity(&self) -> StoreResult<(u64, u64)> {
        self.backing.directory.identity()
    }

    #[cfg(all(test, windows))]
    pub fn open_owned(_: &std::path::Path) -> StoreResult<Self> {
        Err(StoreError::UnsupportedPlatform)
    }

    #[cfg(not(all(test, windows)))]
    pub fn open_owned(path: &std::path::Path) -> StoreResult<Self> {
        Ok(Self {
            backing: StoreRoot {
                directory: Arc::new(disk::Directory::open_owned(path)?),
                _temporary: None,
            },
        })
    }

    /// Opens the root-scoped admission ledger through the same pinned root
    /// capability used by all session writers.
    pub fn session_manager(&self) -> StoreResult<super::SessionManagerStore> {
        super::SessionManagerStore::from_root(self.backing.directory.clone())
    }
    /// Opens one root-run subagent control plane through this already-pinned
    /// desktop root capability.
    pub fn subagent_control(
        &self,
        binding: super::SubagentControlBindingV1,
    ) -> StoreResult<super::SubagentControlOwner> {
        super::SubagentControlOwner::from_root(self.backing.directory.clone(), binding)
    }
    pub fn create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        self.backing.create(project, session, initial)
    }
    pub fn open(&self, project: &ProjectId, session: &SessionId) -> StoreResult<Writer> {
        self.backing.open(project, session)
    }
    /// Only an absent session directory permits initialization, never a missing
    /// marker inside an existing/partially-created session directory.
    pub fn open_or_create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        match self
            .backing
            .directory
            .child(&content_hash(session.as_str().as_bytes()), false)
        {
            Ok(_) => self.open(&project, &session),
            Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.create(project, session, initial)
            }
            Err(e) => Err(e),
        }
    }
}
impl StoreRoot {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn capacity_identity_m2a(&self) -> StoreResult<DesktopCapacityIdentityV1> {
        let (device, inode) = self.directory.identity()?;
        let (allocation_unit, _) = self.directory.capacity_m2a()?;
        if self.directory.identity()? != (device, inode) {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(DesktopCapacityIdentityV1 {
            root_identity: crate::apply_intent::IdentityV1 { device, inode },
            allocation_unit,
        })
    }
    pub fn create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        let state = Sidecar {
            inherited_turn_count: DecimalU64::new(0),
            conversation_memory: None,
            session_revision: DecimalU64::new(0),
            draft: initial.draft,
            configuration: initial.configuration,
            role_bindings: None,
            tier_bindings: None,
            goal: None,
            instructions: Vec::new(),
            policy_revision: initial.policy_revision,
            tasks: Vec::new(),
            children: Vec::new(),
            runs: Vec::new(),
            requests: Vec::new(),
            source_applies: Vec::new(),
            unresolved_approvals: Vec::new(),
            approval_records: Vec::new(),
            workflow: None,
            workflow_artifacts: crate::workflow::WorkflowArtifactGraphV1::default(),
        };
        self.create_seeded(project, session, state, Vec::new())
    }

    fn create_seeded(
        &self,
        project: ProjectId,
        session: SessionId,
        state: Sidecar,
        raw: Vec<RawEventV2>,
    ) -> StoreResult<Writer> {
        let raw_bytes = raw
            .iter()
            .map(disk::line)
            .collect::<StoreResult<Vec<_>>>()?
            .concat();
        let side = disk::line(&state)?;
        let marker = Marker {
            schema_version: 3,
            session_id: session,
            project_id: project,
            epoch: DecimalU64::new(1),
            session_revision: DecimalU64::new(0),
            content_revision: DecimalU64::new(0),
            raw_offset: DecimalU64::new(raw_bytes.len() as u64),
            raw_hash: content_hash(&raw_bytes),
            sidecar_offset: DecimalU64::new(side.len() as u64),
            sidecar_hash: content_hash(&side),
            deleted: false,
        };
        let published = Published { marker, state, raw };
        validate_ledger(&published)?;
        validate_artifact_bodies(self, &published)?;
        self.directory.verify()?;
        let directory = Arc::new(self.directory.child(
            &content_hash(published.marker.session_id.as_str().as_bytes()),
            true,
        )?);
        let guard = directory.open("writer.lock", true, true)?;
        acquire(&guard)?;
        let mut file = directory.open(disk::RAW, true, true)?;
        file.write_all(&raw_bytes)?;
        file.sync_all()?;
        let mut file = directory.open(disk::SIDECAR, true, true)?;
        file.write_all(&side)?;
        file.sync_all()?;
        disk::publish_marker(&directory, &published.marker)?;
        self.directory.sync()?;
        Ok(Writer {
            root: self.clone(),
            directory,
            guard,
            published,
            recovery_required: false,
        })
    }

    /// 再openは同期確認後に非終端runを復旧世代として公開する。自動実行しない。
    pub fn open(&self, project: &ProjectId, session: &SessionId) -> StoreResult<Writer> {
        self.directory.verify()?;
        let directory = Arc::new(
            self.directory
                .child(&content_hash(session.as_str().as_bytes()), false)?,
        );
        let guard = directory.open("writer.lock", true, false)?;
        acquire(&guard)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // Ordinary open validates only the published prefixes. Auxiliary
            // entries and unpublished tails belong to recovery inspection.
            let published = disk::load(&directory, project, session)?;
            validate_ledger(&published)?;
            validate_artifact_bodies_with_mode(self, &published, true)?;
        }
        let published = reconcile(&directory, project, session)?;
        validate_artifact_bodies(self, &published)?;
        self.directory.sync()?;
        if published.marker.deleted {
            return Err(StoreError::Deleted);
        }
        let mut writer = Writer {
            root: self.clone(),
            directory,
            guard,
            published,
            recovery_required: false,
        };
        let mut next = writer.published.clone();
        let mut changed = !next.state.unresolved_approvals.is_empty();
        changed |= invalidate_approvals(&mut next.state, |_| true);
        changed |= super::source_apply::invalidate_unconsumed(&mut next.state);
        // A reopened desktop owner has not re-observed the source/diff/subject
        // hashes. Evidence must therefore be gathered again before it can
        // satisfy an evidence-bearing phase edge.
        if let Some(workflow) = &mut next.state.workflow {
            changed |= workflow.gates.convergence.take().is_some();
        }
        for record in &mut next.state.runs {
            if let Some(workflow) = &mut record.workflow {
                changed |= workflow.gates.convergence.take().is_some();
            }
            if !(record.run.state.is_terminal()
                || record.admission.is_some() && record.run.state == RunState::Queued)
            {
                let unresolved = record.operations.iter().any(mcp::unresolved);
                record.run.state = if unresolved {
                    RunState::OutcomeUnknown
                } else {
                    RunState::Interrupted
                };
                if let Some(memory) = &mut record.memory {
                    memory.phase = polaris_desktop_protocol::snapshot::MemoryPhase::OutcomeUnknown;
                    memory.detail =
                        "The previous run was interrupted. Unresolved summaries will not be resent automatically".into();
                }
                changed = true;
            }
        }
        for child in &mut next.state.children {
            if !child.child.state.is_terminal() {
                child.child.state = RunState::OutcomeUnknown;
                changed = true;
            }
        }
        if changed {
            writer.commit(next, false)?;
        }
        Ok(writer)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn legacy_unbound_notices_m2a(
    published: &Published,
    project_binding: crate::management_effect::ManagementEffectHashV1,
    marker_evidence_hash: crate::management_effect::ManagementEffectHashV1,
    sidecar_prefix_bytes: &[u8],
) -> Result<Vec<LegacyUnboundIntentDiagnosticV1>, D27RecoveryOpenError> {
    use crate::{
        apply_intent::{d27_operation_binding, d27_session_binding},
        management_effect::ManagementEffectHashV1 as Hash,
    };
    let mut legacy_terminal_notices = Vec::new();
    for (index, record) in published.state.source_applies.iter().enumerate() {
        if let Some(intent_revision) = record.intent_revision
            && record.reserved_result_id.is_none()
        {
            let diagnostic = LegacyUnboundIntentDiagnosticV1 {
                schema_version: 1,
                code: "legacy_source_apply_binding_missing",
                project_binding,
                session_binding: d27_session_binding(published.marker.session_id.as_str())
                    .map_err(|_| StoreError::Corrupt("session binding"))?,
                marker_evidence_hash,
                sidecar_prefix_hash: Hash::digest(sidecar_prefix_bytes),
                session_revision: published.marker.session_revision.get(),
                record_index: index as u64,
                intent_revision: intent_revision.get(),
                source_operation_digest: d27_operation_binding(
                    record.candidate.operation_id.as_str(),
                )
                .map_err(|_| StoreError::Corrupt("operation binding"))?,
                result_present: record.result.is_some(),
                next_action: if record.result.is_some() {
                    "preserve_terminal_legacy_record"
                } else {
                    "complete_with_legacy_desktop"
                },
            };
            if record.result.is_none() {
                return Err(D27RecoveryOpenError::LegacyUnboundIntent(Box::new(
                    diagnostic,
                )));
            }
            legacy_terminal_notices.push(diagnostic);
        }
    }
    Ok(legacy_terminal_notices)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_d27_result_capacity_m2a(
    descriptor: &crate::apply_intent::ApplyIntentDescriptorV1,
    consume_bytes: usize,
    result_bytes: usize,
) -> StoreResult<()> {
    let unit = descriptor
        .capacity_plan
        .desktop_allocation_unit
        .ok_or(StoreError::TargetMismatch)?;
    let allocated = |bytes: u64| {
        crate::apply_intent::allocated_bytes(bytes, unit).map_err(|_| StoreError::Overflow)
    };
    let required = allocated(consume_bytes as u64)?
        .checked_add(allocated(result_bytes as u64)?)
        .and_then(|n| {
            n.checked_add(
                allocated(MAX_D27_MARKER_BYTES as u64)
                    .ok()?
                    .checked_mul(2)?,
            )
        })
        .ok_or(StoreError::Overflow)?;
    if required > descriptor.capacity_plan.inventory_commit_bytes {
        return Err(StoreError::Capacity("D27 reserved publication bytes"));
    }
    Ok(())
}

fn prepare_transition_m2a(
    old: &Published,
    mut next: Published,
    content_changed: bool,
) -> StoreResult<Published> {
    super::memory::validate_transition(&old.state, &next.state)?;
    mcp::validate_transition(&old.state, &next.state)?;
    prompt::validate_transition(&old.state, &next.state)?;
    next.marker.session_revision = old.marker.session_revision.checked_add(1)?;
    next.state.session_revision = next.marker.session_revision;
    if content_changed {
        next.marker.content_revision = old.marker.content_revision.checked_add(1)?;
    }
    validate_ledger(&next)?;
    super::source_apply::validate_transition(&old.state, &next.state)?;
    Ok(next)
}

fn acquire(file: &File) -> StoreResult<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Busy),
        Err(std::fs::TryLockError::Error(e)) => Err(StoreError::Io(e)),
    }
}

fn reconcile(
    dir: &disk::Directory,
    project: &ProjectId,
    session: &SessionId,
) -> StoreResult<Published> {
    let published = disk::load(dir, project, session)?;
    validate_ledger(&published)?;
    disk::sync_published(dir)?;
    if disk::read_marker(dir)? != published.marker {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(published)
}

fn validate_artifact_bodies(root: &StoreRoot, published: &Published) -> StoreResult<()> {
    validate_artifact_bodies_with_mode(root, published, false)
}

fn validate_artifact_bodies_with_mode(
    root: &StoreRoot,
    published: &Published,
    readonly: bool,
) -> StoreResult<()> {
    let mut contracts = std::collections::BTreeSet::new();
    let mut features = std::collections::BTreeSet::new();
    let mut tasks = Vec::new();
    let workflows = published
        .state
        .workflow
        .iter()
        .chain(
            published
                .state
                .runs
                .iter()
                .filter_map(|record| record.workflow.as_ref()),
        )
        .collect::<Vec<_>>();
    for workflow in &workflows {
        if let Some(reference) = &workflow.state.active_change_contract {
            contracts.insert(reference.clone());
        }
        if let Some(reference) = &workflow.state.active_feature {
            features.insert(reference.clone());
        }
        if let Some(reference) = &workflow.state.active_task
            && !tasks.contains(reference)
        {
            tasks.push(reference.clone());
        }
    }
    if contracts.is_empty()
        && features.is_empty()
        && tasks.is_empty()
        && workflows
            .iter()
            .all(|workflow| workflow.gates.convergence.is_none())
    {
        return Ok(());
    }

    let store = if readonly {
        super::WorkflowArtifactStore::inspect_from_root_m2a(
            root.directory.clone(),
            &published.marker.project_id,
        )?
    } else {
        super::WorkflowArtifactStore::from_root(
            root.directory.clone(),
            &published.marker.project_id,
        )?
    };
    for reference in contracts {
        let metadata = published
            .state
            .workflow_artifacts
            .metadata(&reference)
            .ok_or(StoreError::Corrupt("workflow artifact graph/body"))?;
        let contract = store.read_change_contract(&reference)?;
        if contract.header != metadata.header || contract.eligibility != metadata.eligibility {
            return Err(StoreError::Corrupt("workflow artifact graph/body"));
        }
    }
    for reference in features {
        store.read_feature_spec(&published.state.workflow_artifacts, &reference)?;
    }
    for reference in tasks {
        let task_graph =
            store.read_task_graph(&published.state.workflow_artifacts, &reference.task_graph)?;
        reference
            .validate_against(&task_graph)
            .map_err(|_| StoreError::Corrupt("workflow active task/body"))?;
    }
    for workflow in workflows {
        if let Some(convergence) = &workflow.gates.convergence {
            validate_owner_convergence_for_workflow(
                &workflow.state,
                &published.state.workflow_artifacts,
                &store,
                convergence,
            )?;
        }
    }
    Ok(())
}

fn build_owner_convergence_for_workflow(
    state: &crate::workflow::WorkflowStateV1,
    graph: &crate::workflow::WorkflowArtifactGraphV1,
    store: &super::WorkflowArtifactStore,
    bundle: &crate::verification::owner::VerifiedConvergenceBundleV1,
) -> StoreResult<ConvergenceInputV1> {
    let observed = ObservedEvidenceSubjectV1 {
        subject_hash: bundle.binding().subject_hash.clone(),
        source_hash: bundle.binding().source_hash.clone(),
        diff_hash: bundle.binding().diff_hash.clone(),
    };
    if let Some(reference) = &state.active_change_contract {
        let contract = store.read_change_contract(reference)?;
        return build_owner_convergence(
            ActiveEvidenceArtifactsV1::RoutineContract {
                artifact_graph: graph,
                active_contract: reference,
                contract: &contract,
            },
            &observed,
            bundle.implementation_receipts().to_vec(),
            bundle.reviews().to_vec(),
            bundle.evidence().to_vec(),
        )
        .and_then(|convergence| {
            if convergence.binding != *bundle.binding() {
                return Err("verification bundle binding is not current".into());
            }
            Ok(convergence)
        })
        .map_err(|_| StoreError::RunConflict);
    }
    let (feature_reference, active_task) =
        match (state.active_feature.as_ref(), state.active_task.as_ref()) {
            (Some(feature), Some(task)) => (feature, task),
            _ => return Err(StoreError::RunConflict),
        };
    let feature = store.read_feature_spec(graph, feature_reference)?;
    let task_graph = store.read_task_graph(graph, &active_task.task_graph)?;
    build_owner_convergence(
        ActiveEvidenceArtifactsV1::PlannedTask {
            artifact_graph: graph,
            active_feature: feature_reference,
            active_task,
            feature: &feature,
            task_graph: &task_graph,
        },
        &observed,
        bundle.implementation_receipts().to_vec(),
        bundle.reviews().to_vec(),
        bundle.evidence().to_vec(),
    )
    .and_then(|convergence| {
        if convergence.binding != *bundle.binding() {
            return Err("verification bundle binding is not current".into());
        }
        Ok(convergence)
    })
    .map_err(|_| StoreError::RunConflict)
}

fn validate_owner_convergence_for_workflow(
    state: &crate::workflow::WorkflowStateV1,
    graph: &crate::workflow::WorkflowArtifactGraphV1,
    store: &super::WorkflowArtifactStore,
    convergence: &ConvergenceInputV1,
) -> StoreResult<()> {
    let observed = ObservedEvidenceSubjectV1 {
        subject_hash: convergence.binding.subject_hash.clone(),
        source_hash: convergence.binding.source_hash.clone(),
        diff_hash: convergence.binding.diff_hash.clone(),
    };
    if let Some(reference) = &state.active_change_contract {
        let contract = store.read_change_contract(reference)?;
        validate_owner_convergence(
            ActiveEvidenceArtifactsV1::RoutineContract {
                artifact_graph: graph,
                active_contract: reference,
                contract: &contract,
            },
            &observed,
            convergence.clone(),
        )
        .map_err(|_| StoreError::Corrupt("workflow convergence/body"))?;
        return Ok(());
    }
    let (feature_reference, active_task) =
        match (state.active_feature.as_ref(), state.active_task.as_ref()) {
            (Some(feature), Some(task)) => (feature, task),
            _ => return Err(StoreError::Corrupt("workflow convergence/active artifact")),
        };
    let feature = store.read_feature_spec(graph, feature_reference)?;
    let task_graph = store.read_task_graph(graph, &active_task.task_graph)?;
    validate_owner_convergence(
        ActiveEvidenceArtifactsV1::PlannedTask {
            artifact_graph: graph,
            active_feature: feature_reference,
            active_task,
            feature: &feature,
            task_graph: &task_graph,
        },
        &observed,
        convergence.clone(),
    )
    .map_err(|_| StoreError::Corrupt("workflow convergence/body"))?;
    Ok(())
}

/// File lockはこのhandleの生存中保持する。Cloneは実装しない。
#[derive(Debug)]
pub struct Writer {
    root: StoreRoot,
    directory: Arc<disk::Directory>,
    guard: File,
    published: Published,
    recovery_required: bool,
}

/// Retains the current writer/root and, for an ancestor, a read-only shared lock.
pub(super) struct MemoryDirectory<'a> {
    pub(super) directory: Arc<disk::Directory>,
    read_guard: Option<File>,
    root: StoreRoot,
    owner: Option<&'a Writer>,
}
impl MemoryDirectory<'_> {
    pub(super) fn verify(&self) -> StoreResult<()> {
        if let Some(owner) = self.owner {
            owner.ready()?;
        }
        self.root.directory.verify()?;
        self.directory.verify()?;
        if let Some(guard) = &self.read_guard {
            disk::same(guard, &self.directory.open("writer.lock", false, false)?)?;
        }
        Ok(())
    }

    /// Move a parent's shared guard across the child's mutable marker commit.
    /// A current-session reader must keep borrowing its exclusive Writer.
    pub(super) fn into_parent(mut self) -> StoreResult<MemoryDirectory<'static>> {
        self.verify()?;
        let guard = self.read_guard.take().ok_or(StoreError::RunConflict)?;
        Ok(MemoryDirectory {
            directory: Arc::clone(&self.directory),
            read_guard: Some(guard),
            root: self.root.clone(),
            owner: None,
        })
    }
}
impl Drop for MemoryDirectory<'_> {
    fn drop(&mut self) {
        if let Some(guard) = &self.read_guard {
            let _ = guard.unlock();
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.guard.unlock();
    }
}

impl Writer {
    pub(super) fn memory_directory(&self, session: &SessionId) -> StoreResult<MemoryDirectory<'_>> {
        self.ready()?;
        let (directory, read_guard) = if session == &self.published.marker.session_id {
            (Arc::clone(&self.directory), None)
        } else {
            let directory = Arc::new(
                self.root
                    .directory
                    .child(&content_hash(session.as_str().as_bytes()), false)?,
            );
            let guard = directory.open("writer.lock", false, false)?;
            match guard.try_lock_shared() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
            (directory, Some(guard))
        };
        let owner = MemoryDirectory {
            directory,
            read_guard,
            root: self.root.clone(),
            owner: Some(self),
        };
        owner.verify()?;
        Ok(owner)
    }

    /// 新規子へ完全な直近原文と固定記憶参照だけを渡す。モデル・RPCは呼ばない。
    pub fn fork_memory(&self, session: SessionId, artifact_hash: &str) -> StoreResult<Writer> {
        self.ready()?;
        let parent = disk::load(&self.directory, self.coordinates().0, self.coordinates().1)?;
        validate_ledger(&parent)?;
        if parent.marker != self.published.marker {
            return Err(StoreError::RecoveryRequired);
        }
        let (state, raw) = super::memory::fork_seed(&parent, &session, artifact_hash)?;
        self.root
            .create_seeded(parent.marker.project_id.clone(), session, state, raw)
    }

    /// Identity of the retained store root, not merely the session coordinates.
    /// Trusted launch uses this to reject a Writer from another directory.
    pub fn root_identity(&self) -> StoreResult<(u64, u64)> {
        self.root.directory.identity()
    }

    /// Immutable owner coordinates, available even when publication needs
    /// recovery. These identify the store; they do not authorize operations.
    pub fn coordinates(&self) -> (&ProjectId, &SessionId) {
        (
            &self.published.marker.project_id,
            &self.published.marker.session_id,
        )
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn retained_root_m2a(&self) -> StoreResult<(std::path::PathBuf, std::fs::File)> {
        self.ready()?;
        Ok((
            self.root.directory.path().to_owned(),
            self.root.directory.retained_file_m2a()?,
        ))
    }
    /// Inspect an ambiguous publication while retaining this same open writer
    /// lock. The duplicate descriptor shares its flock; no second lock or
    /// normal-open reconciliation occurs before the owner joins evidence.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn inspect_retained_recovery_m2a(
        &self,
        project_binding: crate::management_effect::ManagementEffectHashV1,
    ) -> Result<D27RecoveryOpenOutcomeV1, D27RecoveryOpenError> {
        use crate::apply_intent::IdentityV1;
        use std::os::unix::fs::MetadataExt;
        self.root.directory.verify()?;
        self.directory.verify()?;
        disk::same(
            &self.guard,
            &self.directory.open("writer.lock", true, false)?,
        )?;
        let inspected = disk::inspect_published_m2a(
            &self.directory,
            &self.published.marker.project_id,
            &self.published.marker.session_id,
        )?;
        validate_ledger(&inspected.published)?;
        validate_artifact_bodies_with_mode(&self.root, &inspected.published, true)?;
        let (device, inode) = self.root.directory.identity()?;
        let root_identity = IdentityV1 { device, inode };
        let (device, inode) = self.directory.identity()?;
        let session_identity = IdentityV1 { device, inode };
        let lock = self.guard.metadata().map_err(StoreError::Io)?;
        let writer_lock_identity = IdentityV1 {
            device: lock.dev(),
            inode: lock.ino(),
        };
        let raw_offset = inspected.published.marker.raw_offset.get() as usize;
        let sidecar_offset = inspected.published.marker.sidecar_offset.get() as usize;
        let raw_prefix_bytes = inspected.raw_bytes[..raw_offset].to_vec();
        let sidecar_prefix_bytes = inspected.sidecar_bytes[..sidecar_offset].to_vec();
        let marker_evidence_hash =
            marker_evidence_hash(&inspected.marker_bytes, &sidecar_prefix_bytes);
        let legacy_terminal_notices = legacy_unbound_notices_m2a(
            &inspected.published,
            project_binding,
            marker_evidence_hash,
            &sidecar_prefix_bytes,
        )?;
        Ok(D27RecoveryOpenOutcomeV1 {
            view: D27RecoveryInspectV1 {
                root_identity,
                session_identity,
                writer_lock_identity,
                marker_bytes: inspected.marker_bytes,
                marker_evidence_hash,
                raw_prefix_bytes,
                raw_tail_bytes: inspected.raw_bytes[raw_offset..].to_vec(),
                sidecar_prefix_bytes,
                sidecar_tail_bytes: inspected.sidecar_bytes[sidecar_offset..].to_vec(),
                snapshot: inspected.published.clone(),
                state_next: inspected.state_next,
                files: inspected.files,
            },
            legacy_terminal_notices,
            writer: Writer {
                root: self.root.clone(),
                directory: self.directory.clone(),
                guard: self.guard.try_clone().map_err(StoreError::Io)?,
                published: inspected.published,
                recovery_required: true,
            },
        })
    }
    #[cfg(all(test, unix))]
    pub(super) fn test_directory(&self) -> std::path::PathBuf {
        self.directory.path().to_owned()
    }

    #[cfg(all(test, unix))]
    pub(super) fn test_publish_approval(
        &mut self,
        approval: polaris_desktop_protocol::snapshot::PendingApproval,
    ) {
        let mut next = self.published.clone();
        next.state.unresolved_approvals.push(approval);
        self.commit(next, false).unwrap();
    }

    fn ready(&self) -> StoreResult<()> {
        self.root.directory.verify()?;
        self.directory.verify()?;
        disk::same(
            &self.guard,
            &self.directory.open("writer.lock", false, false)?,
        )?;
        if self.recovery_required {
            return Err(StoreError::RecoveryRequired);
        }
        if self.published.marker.deleted {
            return Err(StoreError::Deleted);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> StoreResult<Published> {
        self.ready()?;
        Ok(self.published.clone())
    }

    /// 同handleの曖昧な公開だけを解決する。操作の再dispatchを許可するAPIではない。
    pub fn recover(&mut self) -> StoreResult<()> {
        self.recovery_required = true;
        self.root.directory.verify()?;
        self.directory.verify()?;
        disk::same(
            &self.guard,
            &self.directory.open("writer.lock", false, false)?,
        )?;
        let current = &self.published.marker;
        let published = reconcile(&self.directory, &current.project_id, &current.session_id)?;
        validate_artifact_bodies(&self.root, &published)?;
        self.root.directory.sync()?;
        self.published = published;
        self.recovery_required = false;
        self.ready()
    }

    /// Trusted next-run metadata update, including while a run is active.
    /// Accepted runs retain their snapshot; no endpoint or provider is resolved.
    pub fn configure_role_bindings(
        &mut self,
        expected_configuration_revision: DecimalU64,
        role_bindings: Option<super::SavedRoleBindings>,
        tier_bindings: Option<super::SavedTierBindings>,
        catalog: &[polaris_skills::AgentType],
    ) -> StoreResult<()> {
        self.ready()?;
        cas(
            expected_configuration_revision,
            self.published.state.configuration.configuration_revision,
            "configuration",
        )?;
        if let Some(saved) = &role_bindings {
            saved.validate_catalog(catalog)?;
        }
        if let Some(saved) = &tier_bindings {
            saved.validate()?;
        }
        let mut next = self.published.clone();
        next.state.configuration.configuration_revision = next
            .state
            .configuration
            .configuration_revision
            .checked_add(1)?;
        next.state.role_bindings = role_bindings;
        next.state.tier_bindings = tier_bindings;
        self.commit(next, false)
    }

    /// Trusted controller setting; cannot alter a run already accepted.
    pub fn configure_workflow(
        &mut self,
        expected_session_revision: DecimalU64,
        workflow: Option<super::SavedWorkflow>,
    ) -> StoreResult<()> {
        self.ready()?;
        cas(
            expected_session_revision,
            self.published.marker.session_revision,
            "session",
        )?;
        if self
            .published
            .state
            .runs
            .iter()
            .any(|run| !run.run.state.is_terminal())
        {
            return Err(StoreError::Busy);
        }
        if let Some(saved) = &workflow {
            saved.validate_with_artifacts(&self.published.state.workflow_artifacts)?;
        }
        let mut next = self.published.clone();
        next.state.workflow = workflow;
        self.commit(next, false)
    }

    /// Publish an immutable draft/ready contract body before advancing the
    /// session's metadata graph. This operation does not approve the body or
    /// grant a phase transition.
    pub fn publish_change_contract(
        &mut self,
        expected_session_revision: DecimalU64,
        contract: crate::workflow::ChangeContractV1,
    ) -> StoreResult<crate::workflow::ArtifactRefV1> {
        self.ready()?;
        cas(
            expected_session_revision,
            self.published.marker.session_revision,
            "session",
        )?;
        if !matches!(
            contract.header.status,
            crate::workflow::ArtifactStatusV1::Draft | crate::workflow::ArtifactStatusV1::Ready
        ) {
            return Err(StoreError::RunConflict);
        }
        let reference = contract.reference();
        let mut graph = self.published.state.workflow_artifacts.clone();
        let inserted = graph
            .insert_change_contract(&contract)
            .map_err(|_| StoreError::RunConflict)?;
        let store = super::WorkflowArtifactStore::from_root(
            self.root.directory.clone(),
            &self.published.marker.project_id,
        )?;
        store.put_change_contract(&contract)?;
        if !inserted {
            return Ok(reference);
        }
        let mut next = self.published.clone();
        next.state.workflow_artifacts = graph;
        self.commit(next, false)?;
        Ok(reference)
    }

    /// Trusted approval creates a new immutable revision and publishes its
    /// graph entry together with the compact active reference. Runs already in
    /// flight retain their accepted snapshot, so approval is idle-only.
    pub fn approve_change_contract_trusted(
        &mut self,
        expected_session_revision: DecimalU64,
        reference: &crate::workflow::ArtifactRefV1,
    ) -> StoreResult<crate::workflow::ArtifactRefV1> {
        self.ready()?;
        cas(
            expected_session_revision,
            self.published.marker.session_revision,
            "session",
        )?;
        if self
            .published
            .state
            .runs
            .iter()
            .any(|run| !run.run.state.is_terminal())
        {
            return Err(StoreError::Busy);
        }
        let metadata = self
            .published
            .state
            .workflow_artifacts
            .metadata(reference)
            .ok_or(StoreError::NotFound)?
            .clone();
        let store = super::WorkflowArtifactStore::from_root(
            self.root.directory.clone(),
            &self.published.marker.project_id,
        )?;
        let contract = store.read_change_contract(reference)?;
        if contract.header != metadata.header
            || contract.eligibility != metadata.eligibility
            || !matches!(
                contract.header.status,
                crate::workflow::ArtifactStatusV1::Draft | crate::workflow::ArtifactStatusV1::Ready
            )
        {
            return Err(StoreError::RunConflict);
        }
        let approved = contract
            .revised(crate::workflow::ArtifactStatusV1::Approved)
            .map_err(|_| StoreError::RunConflict)?;
        let approved_reference = approved.reference();
        let mut graph = self.published.state.workflow_artifacts.clone();
        graph
            .insert_change_contract(&approved)
            .map_err(|_| StoreError::RunConflict)?;
        store.put_change_contract(&approved)?;

        let saved = self.published.state.workflow.clone().unwrap_or_else(|| {
            super::SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
                crate::workflow::WorkflowConfig::default(),
            ))
        });
        let mut gates = saved.gates.clone();
        gates.phase_gate_schema_version = Some(crate::workflow::WORKFLOW_ARTIFACT_SCHEMA_VERSION);
        let restored = crate::workflow::SessionWorkflow::restore_with_artifacts(
            saved.config.clone(),
            gates,
            saved.state.clone(),
            graph.clone(),
        )
        .map_err(|_| StoreError::RunConflict)?;
        let workflow = restored
            .with_approved_change_contract(graph.clone(), approved_reference.clone())
            .map_err(|_| StoreError::RunConflict)?;
        let mut next = self.published.clone();
        next.state.workflow_artifacts = graph;
        next.state.workflow = Some(super::SavedWorkflow::capture(&workflow));
        self.commit(next, false)?;
        Ok(approved_reference)
    }

    /// Root run entry point for an ordinary local change. Artifact bodies are
    /// written first and the graph, active ref, session workflow and accepted
    /// run snapshot advance in one marker commit. The model cannot select an
    /// approved status or bypass the deterministic routine predicate.
    pub fn publish_and_approve_routine_contract(
        &mut self,
        target: &RunTarget,
        expected_workflow_revision: u64,
        contract: crate::workflow::ChangeContractV1,
    ) -> StoreResult<crate::workflow::SessionWorkflow> {
        self.ready()?;
        if contract.header.status != crate::workflow::ArtifactStatusV1::Draft
            || !contract.eligibility.is_routine()
        {
            return Err(StoreError::RunConflict);
        }
        let run_index = self.run_index(target)?;
        if self.published.state.runs[run_index].run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let saved = self.published.state.runs[run_index]
            .workflow
            .clone()
            .or_else(|| self.published.state.workflow.clone())
            .unwrap_or_else(|| {
                super::SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
                    crate::workflow::WorkflowConfig::default(),
                ))
            });
        if saved.state.revision != expected_workflow_revision {
            return Err(StoreError::CasConflict("workflow"));
        }
        let store = super::WorkflowArtifactStore::from_root(
            self.root.directory.clone(),
            &self.published.marker.project_id,
        )?;
        let mut graph = self.published.state.workflow_artifacts.clone();
        graph
            .insert_change_contract(&contract)
            .map_err(|_| StoreError::RunConflict)?;
        store.put_change_contract(&contract)?;
        let approved = contract
            .revised(crate::workflow::ArtifactStatusV1::Approved)
            .map_err(|_| StoreError::RunConflict)?;
        let approved_reference = approved.reference();
        graph
            .insert_change_contract(&approved)
            .map_err(|_| StoreError::RunConflict)?;
        store.put_change_contract(&approved)?;

        let mut gates = saved.gates.clone();
        gates.phase_gate_schema_version = Some(crate::workflow::WORKFLOW_ARTIFACT_SCHEMA_VERSION);
        let restored = crate::workflow::SessionWorkflow::restore_with_artifacts(
            saved.config.clone(),
            gates,
            saved.state,
            graph.clone(),
        )
        .map_err(|_| StoreError::RunConflict)?;
        let workflow = restored
            .with_approved_change_contract(graph.clone(), approved_reference)
            .map_err(|_| StoreError::RunConflict)?;
        let captured = super::SavedWorkflow::capture(&workflow);
        let mut next = self.published.clone();
        next.state.workflow_artifacts = graph;
        next.state.workflow = Some(captured.clone());
        next.state.runs[run_index].workflow = Some(captured.clone());
        self.commit(next, false)?;
        Ok(workflow)
    }

    /// The desktop store is the sole writer for a run. It reloads the exact
    /// active body under its lock, derives the required criteria and binding,
    /// and commits the resulting convergence with the run workflow snapshot.
    pub fn publish_convergence_trusted(
        &mut self,
        target: &RunTarget,
        expected_workflow_revision: u64,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> StoreResult<crate::workflow::SessionWorkflow> {
        self.ready()?;
        let run_index = self.run_index(target)?;
        if self.published.state.runs[run_index].run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let saved = self.published.state.runs[run_index]
            .workflow
            .clone()
            .or_else(|| self.published.state.workflow.clone())
            .unwrap_or_else(|| {
                super::SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
                    crate::workflow::WorkflowConfig::default(),
                ))
            });
        if saved.state.revision != expected_workflow_revision {
            return Err(StoreError::CasConflict("workflow"));
        }
        let store = super::WorkflowArtifactStore::from_root(
            self.root.directory.clone(),
            &self.published.marker.project_id,
        )?;
        let convergence = build_owner_convergence_for_workflow(
            &saved.state,
            &self.published.state.workflow_artifacts,
            &store,
            &bundle,
        )?;
        let restored = crate::workflow::SessionWorkflow::restore_with_artifacts(
            saved.config.clone(),
            saved.gates.clone(),
            saved.state,
            self.published.state.workflow_artifacts.clone(),
        )
        .map_err(|_| StoreError::RunConflict)?;
        let workflow = restored
            .with_convergence_trusted(convergence)
            .map_err(|_| StoreError::RunConflict)?;
        let captured = super::SavedWorkflow::capture(&workflow);
        let mut next = self.published.clone();
        next.state.workflow = Some(captured.clone());
        next.state.runs[run_index].workflow = Some(captured);
        self.commit(next, false)?;
        Ok(workflow)
    }

    /// Load only a body referenced by this session's current metadata graph.
    pub fn load_change_contract(
        &self,
        reference: &crate::workflow::ArtifactRefV1,
    ) -> StoreResult<crate::workflow::ChangeContractV1> {
        self.ready()?;
        let metadata = self
            .published
            .state
            .workflow_artifacts
            .metadata(reference)
            .ok_or(StoreError::NotFound)?;
        let store = super::WorkflowArtifactStore::from_root(
            self.root.directory.clone(),
            &self.published.marker.project_id,
        )?;
        let contract = store.read_change_contract(reference)?;
        if contract.header != metadata.header || contract.eligibility != metadata.eligibility {
            return Err(StoreError::Corrupt("workflow artifact graph/body"));
        }
        Ok(contract)
    }

    /// Read one exact immutable workflow body. The caller names the kind and
    /// complete content-addressed reference; there is no "latest" lookup.
    pub fn read_workflow_artifact(
        &self,
        kind: crate::workflow::WorkflowArtifactKindV1,
        reference: &crate::workflow::ArtifactRefV1,
    ) -> StoreResult<serde_json::Value> {
        self.ready()?;
        let graph = &self.published.state.workflow_artifacts;
        let metadata = graph.metadata(reference).ok_or(StoreError::NotFound)?;
        if metadata.kind != kind {
            return Err(StoreError::RunConflict);
        }
        let store = super::WorkflowArtifactStore::from_root(
            self.root.directory.clone(),
            &self.published.marker.project_id,
        )?;
        let value = match kind {
            crate::workflow::WorkflowArtifactKindV1::ProjectPrinciples => {
                serde_json::to_value(store.read_project_principles(graph, reference)?)?
            }
            crate::workflow::WorkflowArtifactKindV1::IdeaDecision => {
                serde_json::to_value(store.read_idea_decision(graph, reference)?)?
            }
            crate::workflow::WorkflowArtifactKindV1::ChangeContract => {
                serde_json::to_value(self.load_change_contract(reference)?)?
            }
            crate::workflow::WorkflowArtifactKindV1::FeatureSpec => {
                serde_json::to_value(store.read_feature_spec(graph, reference)?)?
            }
            crate::workflow::WorkflowArtifactKindV1::ImplementationPlan => {
                serde_json::to_value(store.read_implementation_plan(graph, reference)?)?
            }
            crate::workflow::WorkflowArtifactKindV1::TaskGraph => {
                serde_json::to_value(store.read_task_graph(graph, reference)?)?
            }
        };
        Ok(value)
    }

    fn prepare_commit(&self, next: Published, content_changed: bool) -> StoreResult<Published> {
        self.ready()?;
        prepare_transition_m2a(&self.published, next, content_changed)
    }

    pub(super) fn commit(&mut self, next: Published, content_changed: bool) -> StoreResult<()> {
        let mut next = self.prepare_commit(next, content_changed)?;
        // 事前失敗も含め停止し、未公開tailを次の変更に混ぜない。
        self.recovery_required = true;
        next.marker = disk::commit(&self.directory, &self.published, &next)?;
        self.root.directory.verify()?;
        self.directory.verify()?;
        self.published = next;
        self.recovery_required = false;
        Ok(())
    }

    pub(super) fn memory_raw_prefix(&self, offset: DecimalU64, hash: &str) -> StoreResult<Vec<u8>> {
        self.ready()?;
        if offset > self.published.marker.raw_offset {
            return Err(StoreError::Corrupt("Memory source range"));
        }
        disk::prefix(&self.directory, disk::RAW, offset, hash)
    }

    fn acceptance(&self, record: &RequestRecord) -> Acceptance {
        let run = match &record.result {
            RequestResult::RunAccepted { run_id, attempt_id }
            | RequestResult::CancelRequested { run_id, attempt_id } => self
                .published
                .state
                .runs
                .iter()
                .find(|r| &r.run.run_id == run_id && &r.run.attempt_id == attempt_id)
                .cloned(),
            _ => None,
        };
        Acceptance {
            record: record.clone(),
            run,
        }
    }

    pub fn request_status(
        &self,
        session: &SessionId,
        client: &ClientId,
        request: &RequestId,
    ) -> StoreResult<Option<Acceptance>> {
        self.ready()?;
        if session != &self.published.marker.session_id {
            return Err(StoreError::TargetMismatch);
        }
        Ok(self
            .published
            .state
            .requests
            .iter()
            .find(|r| &r.client_id == client && &r.request_id == request)
            .map(|r| self.acceptance(r)))
    }

    /// P1の型付き要求を決定的に直列化し、dedupをCASより先に照合する。
    /// run ID/attemptは新規RunStartでだけ必要。同じ要求の再送では無視する。
    pub fn apply(
        &mut self,
        request: &Request,
        target: Option<RunTarget>,
    ) -> StoreResult<Acceptance> {
        self.apply_bound(request, target, None)
    }

    /// The same acceptance, with the registration the run is bound to. The
    /// binding is written by the acceptance itself, never by a later save.
    pub fn apply_bound(
        &mut self,
        request: &Request,
        target: Option<RunTarget>,
        account: Option<super::SavedAccount>,
    ) -> StoreResult<Acceptance> {
        if let Some(account) = &account {
            account.validate()?;
        }
        self.ready()?;
        if request.body.session_id() != Some(&self.published.marker.session_id) {
            return Err(StoreError::TargetMismatch);
        }
        if !matches!(
            request.body,
            RequestBody::DraftUpdate(..)
                | RequestBody::SessionConfigure(..)
                | RequestBody::GoalUpdate(..)
                | RequestBody::InstructionSend(..)
                | RequestBody::RunStart(..)
                | RequestBody::RunCancel(..)
        ) {
            return Err(StoreError::UnsupportedMethod);
        }
        let hash = content_hash(&serde_json::to_vec(request)?);
        if let Some(existing) = self.request_status(
            &self.published.marker.session_id,
            &request.client_id,
            &request.request_id,
        )? {
            if existing.record.request_hash != hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let mut next = self.published.clone();
        let state = &mut next.state;
        let content_changed = matches!(request.body, RequestBody::RunStart(..));
        let result =
            match &request.body {
                RequestBody::DraftUpdate(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    cas(
                        params.expected_draft_revision,
                        state.draft.draft_revision,
                        "draft",
                    )?;
                    state.draft = Draft {
                        draft_revision: state.draft.draft_revision.checked_add(1)?,
                        text: params.text.clone(),
                        attachment_ids: params.attachment_ids.clone(),
                    };
                    RequestResult::DraftUpdated {
                        draft_revision: state.draft.draft_revision,
                    }
                }
                RequestBody::SessionConfigure(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    cas(
                        params.expected_configuration_revision,
                        state.configuration.configuration_revision,
                        "configuration",
                    )?;
                    state.configuration.configuration_revision =
                        state.configuration.configuration_revision.checked_add(1)?;
                    state.configuration.provider = params.provider.clone();
                    state.configuration.model = params.model.clone();
                    state.configuration.effort = params.effort.clone();
                    state.configuration.history_mode = params.history_mode;
                    RequestResult::Configured {
                        configuration_revision: state.configuration.configuration_revision,
                        configuration: state.configuration.clone(),
                    }
                }
                RequestBody::GoalUpdate(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    use polaris_desktop_protocol::request::GoalOperation;
                    let previous = state.goal.clone();
                    let goal = match (&params.operation, previous) {
                        (GoalOperation::Set { text }, previous) => {
                            if text.trim().is_empty() || text.len() > super::goal::MAX_GOAL_BYTES {
                                return Err(StoreError::Corrupt("goal"));
                            }
                            super::SavedGoal {
                                text: text.clone(),
                                revision: DecimalU64::new(
                                    previous
                                        .as_ref()
                                        .map_or(0, |goal| goal.revision.get())
                                        .checked_add(1)
                                        .ok_or(StoreError::Overflow)?,
                                ),
                                active: true,
                                // A new goal never clears an explicit stop.
                                stopped: previous.is_some_and(|goal| goal.stopped),
                                automatic_used: DecimalU64::new(0),
                            }
                        }
                        (_, None) => return Err(StoreError::NotFound),
                        (operation, Some(previous)) => super::SavedGoal {
                            revision: previous.revision.checked_add(1)?,
                            active: !matches!(operation, GoalOperation::Clear) && previous.active,
                            stopped: match operation {
                                GoalOperation::Stop => true,
                                GoalOperation::Resume => false,
                                _ => previous.stopped,
                            },
                            ..previous
                        },
                    };
                    goal.validate()?;
                    let goal_revision = goal.revision;
                    state.goal = Some(goal);
                    RequestResult::GoalUpdated { goal_revision }
                }
                RequestBody::InstructionSend(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    if state.instructions.len() >= super::goal::MAX_INSTRUCTIONS {
                        // Applied instructions are already in a run's history,
                        // so the oldest of them makes room. A queue of
                        // instructions nobody took is refused instead.
                        let applied = state
                            .instructions
                            .iter()
                            .position(|held| held.applied_to.is_some())
                            .ok_or(StoreError::Overflow)?;
                        state.instructions.remove(applied);
                    }
                    // The request identity is the instruction identity, so a
                    // resend can never become a second instruction.
                    let instruction = super::SavedInstruction {
                        instruction_id: format!("i-{}", request.request_id.as_str()),
                        text: params.text.clone(),
                        received_revision: state.session_revision.checked_add(1)?,
                        applied_to: None,
                    };
                    instruction.validate()?;
                    if state
                        .instructions
                        .iter()
                        .any(|held| held.instruction_id == instruction.instruction_id)
                    {
                        return Err(StoreError::RequestConflict);
                    }
                    let instruction_id = instruction.instruction_id.clone();
                    state.instructions.push(instruction);
                    RequestResult::InstructionReceived { instruction_id }
                }
                RequestBody::RunStart(_, params) => {
                    if let Some(choice) = &params.mcp_prompt {
                        prompt::validate_choice(choice)?;
                    }
                    if state.runs.iter().any(|r| !r.run.state.is_terminal()) {
                        return Err(StoreError::Busy);
                    }
                    cas(
                        params.expected_draft_revision,
                        state.draft.draft_revision,
                        "draft",
                    )?;
                    cas(
                        params.expected_configuration_revision,
                        state.configuration.configuration_revision,
                        "configuration",
                    )?;
                    cas(
                        params.expected_policy_revision,
                        state.policy_revision,
                        "policy",
                    )?;
                    let target = target.ok_or(StoreError::RunConflict)?;
                    if state.runs.iter().any(|r| {
                        r.run.run_id == target.run_id || r.run.attempt_id == target.attempt_id
                    }) {
                        return Err(StoreError::RunConflict);
                    }
                    let prior = next.raw.last();
                    next.raw.push(RawEventV2 {
                        schema_version: 2,
                        sequence: prior.map_or(Ok(1), |p| {
                            p.sequence.checked_add(1).ok_or(StoreError::Overflow)
                        })?,
                        epoch: next.marker.epoch.get(),
                        turn_id: prior.map_or(Ok(1), |p| {
                            p.turn_id.checked_add(1).ok_or(StoreError::Overflow)
                        })?,
                        starts_turn: true,
                        message: Message::user(state.draft.text.clone()),
                    });
                    state.runs.push(RunRecord {
                        run: Run {
                            run_id: target.run_id.clone(),
                            attempt_id: target.attempt_id.clone(),
                            state: RunState::Queued,
                            task_ids: Vec::new(),
                        },
                        admission: None,
                        configuration: state.configuration.clone(),
                        role_bindings: state.role_bindings.clone(),
                        tier_bindings: state.tier_bindings.clone(),
                        account,
                        policy_revision: state.policy_revision,
                        input: state.draft.clone(),
                        mcp_prompt: params.mcp_prompt.clone(),
                        operations: Vec::new(),
                        result_id: None,
                        usage: None,
                        memory: None,
                        memory_resources: None,
                        workflow: state.workflow.clone(),
                        history_gap: None,
                    });
                    state.draft = Draft {
                        draft_revision: state.draft.draft_revision.checked_add(1)?,
                        text: String::new(),
                        attachment_ids: Vec::new(),
                    };
                    RequestResult::RunAccepted {
                        run_id: target.run_id,
                        attempt_id: target.attempt_id,
                    }
                }
                RequestBody::RunCancel(_, cancel) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    let record = state
                        .runs
                        .iter_mut()
                        .find(|r| {
                            r.run.run_id == cancel.run_id && r.run.attempt_id == cancel.attempt_id
                        })
                        .ok_or(StoreError::NotFound)?;
                    let before = record.run.state;
                    record.run.state = before
                        .transition(RunInput::Control(Control::Cancel))
                        .map_err(|_| StoreError::RunConflict)?
                        .state();
                    // 未開始の取消には操作結果がなく、台帳と終端を同時公開できる。
                    if before == RunState::Queued {
                        record.result_id = Some(ResultId::new(format!("cancel-{hash}"))?);
                    }
                    invalidate_approvals(state, |a| {
                        a.run_id == cancel.run_id && a.attempt_id == cancel.attempt_id
                    });
                    RequestResult::CancelRequested {
                        run_id: cancel.run_id.clone(),
                        attempt_id: cancel.attempt_id.clone(),
                    }
                }
                _ => return Err(StoreError::UnsupportedMethod),
            };
        let record = RequestRecord {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            accepted_revision: next.marker.session_revision.checked_add(1)?,
            result,
        };
        state.requests.push(record.clone());
        self.commit(next, content_changed)?;
        Ok(self.acceptance(&record))
    }

    /// Atomically appends one manager-admitted user input and its queued run.
    ///
    /// The admission binding is part of the same sidecar generation as the
    /// raw user message.  A recovered manager can therefore advance from
    /// `DestinationReady` without ever sending or appending the input again.
    /// Replaying the same admission is a read; reusing its ID for another
    /// payload or destination fails closed.
    pub fn append_admitted_run(
        &mut self,
        admission: &crate::session_manager::AdmissionV1,
        target: RunTarget,
        account: Option<super::SavedAccount>,
    ) -> StoreResult<AdmittedRun> {
        if let Some(account) = &account {
            account.validate()?;
        }
        self.ready()?;
        let destination = match &admission.state {
            crate::session_manager::AdmissionState::DestinationReady { session_id, .. }
                if session_id.as_str() == self.published.marker.session_id.as_str() =>
            {
                session_id
            }
            _ => return Err(StoreError::TargetMismatch),
        };
        if let Some(existing) = self.published.state.runs.iter().find(|record| {
            record
                .admission
                .as_ref()
                .is_some_and(|saved| saved.admission_id == admission.id.as_str())
        }) {
            let saved = existing
                .admission
                .as_ref()
                .ok_or(StoreError::Corrupt("admission binding"))?;
            let attachments = admission
                .payload
                .attachment_ids
                .iter()
                .map(|id| {
                    AttachmentId::new(id.as_str()).map_err(|_| StoreError::Corrupt("attachment"))
                })
                .collect::<StoreResult<Vec<_>>>()?;
            if saved.payload_hash != admission.payload_hash
                || saved.destination_session_id != self.published.marker.session_id
                || existing.input.text != admission.payload.text
                || existing.input.attachment_ids != attachments
            {
                return Err(StoreError::RequestConflict);
            }
            return Ok(AdmittedRun {
                run: existing.clone(),
                newly_appended: false,
            });
        }
        if self.published.state.runs.iter().any(|record| {
            record.run.run_id == target.run_id || record.run.attempt_id == target.attempt_id
        }) {
            return Err(StoreError::RunConflict);
        }
        let attachments = admission
            .payload
            .attachment_ids
            .iter()
            .map(|id| AttachmentId::new(id.as_str()).map_err(|_| StoreError::Corrupt("attachment")))
            .collect::<StoreResult<Vec<_>>>()?;
        let consumed_draft_revision = self.published.state.draft.draft_revision;
        let consumes_current_draft = self.published.marker.session_revision.get()
            == admission.payload.expected_session_revision
            && self.published.state.draft.text == admission.payload.text
            && self.published.state.draft.attachment_ids == attachments;
        let input = Draft {
            draft_revision: consumed_draft_revision,
            text: admission.payload.text.clone(),
            attachment_ids: attachments.clone(),
        };
        let mut next = self.published.clone();
        let prior = next.raw.last();
        next.raw.push(RawEventV2 {
            schema_version: 2,
            sequence: prior.map_or(Ok(1), |event| {
                event.sequence.checked_add(1).ok_or(StoreError::Overflow)
            })?,
            epoch: next.marker.epoch.get(),
            turn_id: prior.map_or(Ok(1), |event| {
                event.turn_id.checked_add(1).ok_or(StoreError::Overflow)
            })?,
            starts_turn: true,
            message: Message::user(input.text.clone()),
        });
        let state = &mut next.state;
        let record = RunRecord {
            run: Run {
                run_id: target.run_id,
                attempt_id: target.attempt_id,
                state: RunState::Queued,
                task_ids: Vec::new(),
            },
            admission: Some(SavedAdmissionBinding {
                admission_id: admission.id.as_str().to_owned(),
                payload_hash: admission.payload_hash.clone(),
                destination_session_id: self.published.marker.session_id.clone(),
            }),
            configuration: state.configuration.clone(),
            role_bindings: state.role_bindings.clone(),
            tier_bindings: state.tier_bindings.clone(),
            account,
            policy_revision: state.policy_revision,
            input,
            mcp_prompt: None,
            operations: Vec::new(),
            result_id: None,
            usage: None,
            memory: None,
            memory_resources: None,
            workflow: state.workflow.clone(),
            history_gap: None,
        };
        state.runs.push(record.clone());
        if consumes_current_draft && state.draft.draft_revision == consumed_draft_revision {
            state.draft = Draft {
                draft_revision: state.draft.draft_revision.checked_add(1)?,
                text: String::new(),
                attachment_ids: Vec::new(),
            };
        }
        self.commit(next, true)?;
        let _ = destination;
        Ok(AdmittedRun {
            run: record,
            newly_appended: true,
        })
    }

    pub fn admitted_run(&self, admission_id: &str) -> StoreResult<Option<RunRecord>> {
        self.ready()?;
        Ok(self
            .published
            .state
            .runs
            .iter()
            .find(|record| {
                record
                    .admission
                    .as_ref()
                    .is_some_and(|binding| binding.admission_id == admission_id)
            })
            .cloned())
    }

    /// NewlyPublishedだけが新しい開始意図の耐久保存を示す。実際の認可は上位層の責務。
    pub fn record_intent(
        &mut self,
        target: &RunTarget,
        operation: OperationId,
    ) -> StoreResult<IntentReceipt> {
        let (next, receipt) = self.prepare_unapproved_intent(target, operation)?;
        if receipt == IntentReceipt::NewlyPublished {
            self.commit(next, false)?;
        }
        Ok(receipt)
    }

    fn prepare_unapproved_intent(
        &self,
        target: &RunTarget,
        operation: OperationId,
    ) -> StoreResult<(Published, IntentReceipt)> {
        self.ready()?;
        if self
            .published
            .state
            .approval_records
            .iter()
            .any(|r| r.pending.operation_id == operation)
        {
            return Err(StoreError::RunConflict);
        }
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record
            .operations
            .iter()
            .any(|o| o.operation_id == operation)
        {
            return Ok((self.published.clone(), IntentReceipt::AlreadyRecorded));
        }
        if !record.run.state.permits_start()
            || self
                .published
                .state
                .runs
                .iter()
                .any(|r| r.operations.iter().any(|o| o.operation_id == operation))
        {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        let record = &mut next.state.runs[index];
        if record.run.state != RunState::Running {
            record.run.state = record
                .run
                .state
                .transition(RunInput::Control(Control::Start))
                .map_err(|_| StoreError::RunConflict)?
                .state();
        }
        record.operations.push(Operation {
            operation_id: operation,
            result_id: None,
            mcp: None,
            prompt: None,
            recipe: None,
        });
        Ok((next, IntentReceipt::NewlyPublished))
    }

    pub fn record_operation_result(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        result: ResultId,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        let op = record
            .operations
            .iter()
            .position(|o| &o.operation_id == operation)
            .ok_or(StoreError::NotFound)?;
        if let Some(existing) = &record.operations[op].result_id {
            return if existing == &result {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.runs[index].operations[op].result_id = Some(result);
        self.commit(next, false)
    }

    /// Publish the owner's cumulative meter, including failure or cancellation.
    /// Repeated checkpoints do not add counts; older observations cannot erase them.
    pub fn record_usage(
        &mut self,
        target: &RunTarget,
        report: polaris_provider::UsageReport,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        if let Some(prior) = self.published.state.runs[index].usage {
            if prior == report {
                return Ok(());
            }
            if report.reported_responses < prior.reported_responses
                || report.missing_responses < prior.missing_responses
                || report.failed_requests < prior.failed_requests
                || report.usage.input_tokens < prior.usage.input_tokens
                || report.usage.output_tokens < prior.usage.output_tokens
                || report.usage.total_tokens < prior.usage.total_tokens
                || report.usage.cached_tokens < prior.usage.cached_tokens
            {
                return Err(StoreError::RunConflict);
            }
        }
        let mut next = self.published.clone();
        next.state.runs[index].usage = Some(report);
        self.commit(next, false)
    }

    /// Publish a bounded diagnostic before terminal publication. If this fails,
    /// the owner must retain recovery-required state and cannot announce ready.
    /// Sets or replaces the goal. A stop stays until an explicit resume, so a
    /// new goal does not silently restart anything.
    pub fn set_goal(&mut self, text: String) -> StoreResult<u64> {
        self.ready()?;
        if text.trim().is_empty() || text.len() > super::goal::MAX_GOAL_BYTES {
            return Err(StoreError::Corrupt("goal"));
        }
        let mut next = self.published.clone();
        let revision = next
            .state
            .goal
            .as_ref()
            .map_or(0, |goal| goal.revision.get());
        let goal = super::SavedGoal {
            text,
            revision: DecimalU64::new(revision.checked_add(1).ok_or(StoreError::Overflow)?),
            active: true,
            stopped: next.state.goal.as_ref().is_some_and(|goal| goal.stopped),
            automatic_used: DecimalU64::new(0),
        };
        goal.validate()?;
        let revision = goal.revision.get();
        next.state.goal = Some(goal);
        self.commit(next, false)?;
        Ok(revision)
    }

    /// Clears, stops or resumes the goal. Clearing keeps the revision so an
    /// older run cannot be treated as still pursuing it.
    pub fn update_goal(&mut self, active: bool, stopped: bool) -> StoreResult<u64> {
        self.ready()?;
        let mut next = self.published.clone();
        let goal = next.state.goal.as_mut().ok_or(StoreError::NotFound)?;
        goal.revision = goal.revision.checked_add(1)?;
        goal.active = active && !goal.text.trim().is_empty();
        goal.stopped = stopped;
        let revision = goal.revision.get();
        let goal = goal.clone();
        goal.validate()?;
        self.commit(next, false)?;
        Ok(revision)
    }

    /// Spends one automatic continuation. Refused once the budget is gone,
    /// so a restart cannot hand out a second one.
    pub fn record_automatic_resume(&mut self) -> StoreResult<u64> {
        self.ready()?;
        let mut next = self.published.clone();
        let goal = next.state.goal.as_mut().ok_or(StoreError::NotFound)?;
        if goal.automatic_used.get() >= crate::resume::MAX_AUTOMATIC {
            return Err(StoreError::RunConflict);
        }
        goal.automatic_used = goal.automatic_used.checked_add(1)?;
        let used = goal.automatic_used.get();
        self.commit(next, false)?;
        Ok(used)
    }

    /// Durable on receipt. Applying it is a separate, later transition.
    pub fn receive_instruction(&mut self, id: String, text: String) -> StoreResult<()> {
        self.ready()?;
        let mut next = self.published.clone();
        if next.state.instructions.len() >= super::goal::MAX_INSTRUCTIONS {
            let applied = next
                .state
                .instructions
                .iter()
                .position(|held| held.applied_to.is_some())
                .ok_or(StoreError::Overflow)?;
            next.state.instructions.remove(applied);
        }
        if next
            .state
            .instructions
            .iter()
            .any(|instruction| instruction.instruction_id == id)
        {
            // The same instruction is never stored twice, so a resend cannot
            // be applied twice either.
            return Ok(());
        }
        let instruction = super::SavedInstruction {
            instruction_id: id,
            text,
            received_revision: next.state.session_revision.checked_add(1)?,
            applied_to: None,
        };
        instruction.validate()?;
        next.state.instructions.push(instruction);
        self.commit(next, false)
    }

    /// Marks exactly the instructions a run took in, at its apply boundary.
    pub fn apply_instructions(&mut self, target: &RunTarget, ids: &[String]) -> StoreResult<()> {
        self.ready()?;
        self.run_index(target)?;
        let mut next = self.published.clone();
        for id in ids {
            let instruction = next
                .state
                .instructions
                .iter_mut()
                .find(|instruction| &instruction.instruction_id == id)
                .ok_or(StoreError::NotFound)?;
            // The same attempt never applies one twice; a later attempt may
            // take over one an abandoned attempt had taken.
            if instruction
                .applied_to
                .as_ref()
                .is_some_and(|applied| applied.attempt_id == target.attempt_id)
            {
                return Err(StoreError::RunConflict);
            }
            instruction.applied_to = Some(super::AppliedInstruction {
                run_id: target.run_id.clone(),
                attempt_id: target.attempt_id.clone(),
            });
        }
        self.commit(next, false)
    }

    pub fn record_history_gap(&mut self, target: &RunTarget, gap: HistoryGap) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.history_gap == Some(gap) {
            return Ok(());
        }
        if record.run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.runs[index].history_gap = Some(gap);
        self.commit(next, false)
    }

    /// 全開始意図の結果参照を確認し、終端と結果参照を一緒に公開する。
    pub fn finish(
        &mut self,
        target: &RunTarget,
        outcome: Observation,
        result: ResultId,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.history_gap.is_some() && outcome == Observation::Succeeded {
            return Err(StoreError::RunConflict);
        }
        if record.run.state.is_terminal() {
            return if record.run.state == outcome.state()
                && record.result_id.as_ref() == Some(&result)
            {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.operations.iter().any(|o| o.result_id.is_none())
            && outcome != Observation::OutcomeUnknown
        {
            return Err(StoreError::RunConflict);
        }
        let state = record
            .run
            .state
            .transition(RunInput::Observed(outcome))
            .map_err(|_| StoreError::RunConflict)?
            .state();
        let mut next = self.published.clone();
        super::children::seal(&mut next.state, target, outcome)?;
        next.state.runs[index].run.state = state;
        next.state.runs[index].result_id = Some(result);
        next.state
            .unresolved_approvals
            .retain(|a| a.run_id != target.run_id || a.attempt_id != target.attempt_id);
        invalidate_approvals(&mut next.state, |a| {
            a.run_id == target.run_id && a.attempt_id == target.attempt_id
        });
        self.commit(next, false)
    }

    /// Persist a child start before exposing its event. Replays never authorize spawning.
    pub fn record_child_start(
        &mut self,
        target: &RunTarget,
        mut child: polaris_desktop_protocol::snapshot::Child,
        agent_type: String,
        task: String,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let index = self.run_index(target)?;
        if child.state != RunState::Running {
            return Err(StoreError::RunConflict);
        }
        if let Some(existing) = self
            .published
            .state
            .children
            .iter()
            .find(|c| c.child.run_id == child.run_id)
        {
            if child
                .display_name
                .as_ref()
                .is_some_and(|name| existing.child.display_name.as_ref() != Some(name))
            {
                return Err(StoreError::RunConflict);
            }
            child.display_name = existing.child.display_name.clone();
            let saved = super::SavedChild {
                root: target.clone(),
                child,
                agent_type,
                task,
            };
            let mut replay = existing.clone();
            replay.child.state = RunState::Running;
            return if replay == saved {
                Ok(IntentReceipt::AlreadyRecorded)
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if child.display_name.is_some() {
            return Err(StoreError::RunConflict);
        }
        let root_state = self.published.state.runs[index].run.state;
        if root_state == RunState::Queued || root_state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let used_names = self
            .published
            .state
            .children
            .iter()
            .filter(|saved| saved.root == *target || !saved.child.state.is_terminal())
            .filter_map(|saved| saved.child.display_name.as_deref());
        let display_name =
            super::star_names::first_available(used_names).ok_or(StoreError::RunConflict)?;
        child.display_name = Some(display_name.to_owned());
        let saved = super::SavedChild {
            root: target.clone(),
            child,
            agent_type,
            task,
        };
        let mut next = self.published.clone();
        next.state.children.push(saved);
        super::children::validate(&next.state).map_err(|_| StoreError::RunConflict)?;
        self.commit(next, false)?;
        Ok(IntentReceipt::NewlyPublished)
    }

    /// Call after observing the child's terminal, or after joining when its result is unknown.
    /// A receipt is returned only after durable publication; task acceptance is unchanged.
    pub fn record_child_finish(
        &mut self,
        target: &RunTarget,
        identity: &crate::desktop_events::ChildIdentity,
        outcome: Observation,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let root = self.run_index(target)?;
        let index = self
            .published
            .state
            .children
            .iter()
            .position(|c| {
                c.root == *target
                    && c.child.run_id == identity.run_id
                    && c.child.attempt_id == identity.attempt_id
                    && c.child.parent_run_id == identity.parent_run_id
            })
            .ok_or(StoreError::RunConflict)?;
        let state = self.published.state.children[index].child.state;
        if state.is_terminal() {
            return if state == outcome.state() {
                Ok(IntentReceipt::AlreadyRecorded)
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if self.published.state.runs[root].run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.children[index].child.state = outcome.state();
        self.commit(next, false)?;
        Ok(IntentReceipt::NewlyPublished)
    }

    /// 所有者が観測した単一操作の結果・assistant原文・終端を同じmarkerで公開する。
    /// fake以外の操作を一括で成功扱いにせず、指定外の未解決意図は拒否する。
    pub fn finish_with_text(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        outcome: Observation,
        result: ResultId,
        text: String,
    ) -> StoreResult<()> {
        self.finish_with_messages(
            target,
            operation,
            outcome,
            result,
            vec![Message::assistant(text)],
        )
    }

    /// Publish the observed assistant/tool suffix with its terminal marker.
    /// The accepted user message is already durable; callers must not submit it again.
    /// Incomplete tool calls may survive an interrupted turn, but never a success.
    pub fn finish_with_messages(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        outcome: Observation,
        result: ResultId,
        messages: Vec<Message>,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let workflow = self.published.state.runs[index].workflow.clone();
        self.finish_with_messages_and_workflow(
            target, operation, outcome, result, messages, workflow,
        )
    }

    /// Atomically acknowledge one workflow transition and its complete raw
    /// suffix. The worker always supplies the complete suffix; this owner
    /// appends only the tail beyond its durable cursor.
    pub fn checkpoint_workflow(
        &mut self,
        target: &RunTarget,
        full_turn_suffix: Vec<Message>,
        previous: super::SavedWorkflow,
        candidate: super::SavedWorkflow,
        tool_call_id: Option<&str>,
        gate_basis_hash: Option<&str>,
    ) -> StoreResult<()> {
        self.ready()?;
        previous.validate_with_artifacts(&self.published.state.workflow_artifacts)?;
        candidate.validate_with_artifacts(&self.published.state.workflow_artifacts)?;
        validate_turn_suffix(&full_turn_suffix, Observation::Cancelled)?;
        if let Some(id) = tool_call_id
            && !full_turn_suffix
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id))
        {
            return Err(StoreError::RunConflict);
        }
        if previous.config != candidate.config
            || !same_or_staled_gates(&previous.gates, &candidate.gates)
            || previous.state.active_change_contract != candidate.state.active_change_contract
            || (candidate.state.revision == previous.state.revision
                && candidate.state != previous.state)
            || (candidate.state.revision != previous.state.revision
                && candidate.state.revision
                    != previous
                        .state
                        .revision
                        .checked_add(1)
                        .ok_or(StoreError::Overflow)?)
        {
            return Err(StoreError::RunConflict);
        }
        let phase_changed = previous.state.phase != candidate.state.phase;
        if phase_changed || gate_basis_hash.is_some() {
            // The graph hash does not prove that the immutable body still
            // exists. Revalidate it under the writer lock immediately before
            // evaluating the phase gate.
            validate_artifact_bodies(&self.root, &self.published)?;
        }
        if phase_changed && previous.gates.phase_gate_schema_version.is_some() {
            let supplied = gate_basis_hash.ok_or(StoreError::RunConflict)?;
            let proof = crate::workflow::evaluate_phase_transition(
                &previous.state,
                candidate.state.phase,
                &previous.gates,
                &self.published.state.workflow_artifacts,
            )
            .map_err(|_| StoreError::RunConflict)?;
            if proof.basis_hash != supplied {
                return Err(StoreError::RunConflict);
            }
        } else if let Some(supplied) = gate_basis_hash {
            let proof = crate::workflow::evaluate_phase_transition(
                &previous.state,
                candidate.state.phase,
                &previous.gates,
                &self.published.state.workflow_artifacts,
            )
            .map_err(|_| StoreError::RunConflict)?;
            if proof.basis_hash != supplied {
                return Err(StoreError::RunConflict);
            }
        }
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.run.state.is_terminal() {
            return if record.workflow.as_ref() == Some(&candidate)
                && same_messages(
                    &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                    &full_turn_suffix,
                )? {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.workflow.as_ref() == Some(&candidate)
            && same_messages(
                &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                &full_turn_suffix,
            )?
        {
            return Ok(());
        }
        let saved = record.workflow.as_ref().ok_or(StoreError::RunConflict)?;
        if !checkpoint_base_matches(saved, &previous) {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        let tail = turn_suffix_tail(&next, next.raw_run_index(index)?, &full_turn_suffix)?;
        let raw_index = next.raw_run_index(index)?;
        append_turn_suffix(&mut next.raw, raw_index, tail)?;
        next.state.runs[index].workflow = Some(candidate.clone());
        next.state.workflow = Some(candidate);
        self.commit(next, true)
    }

    /// Reconcile the durable marker after an uncertain ACK, without dispatching
    /// or appending anything. The owner handle binds project/session/epoch;
    /// target plus the exact saved workflow and full suffix bind the checkpoint.
    pub fn confirm_workflow_checkpoint(
        &mut self,
        target: &RunTarget,
        full_turn_suffix: &[Message],
        candidate: &super::SavedWorkflow,
    ) -> StoreResult<()> {
        self.recover()?;
        let index = self.run_index(target)?;
        if self.published.state.runs[index].workflow.as_ref() == Some(candidate)
            && same_messages(
                &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                full_turn_suffix,
            )?
        {
            Ok(())
        } else {
            Err(StoreError::RunConflict)
        }
    }

    /// Publish raw output and workflow evidence under the same durable marker.
    /// Replay compares the checkpoint belonging to this run, not a newer turn.
    pub fn finish_with_messages_and_workflow(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        outcome: Observation,
        result: ResultId,
        messages: Vec<Message>,
        workflow: Option<super::SavedWorkflow>,
    ) -> StoreResult<()> {
        self.ready()?;
        if let Some(saved) = &workflow {
            saved.validate_with_artifacts(&self.published.state.workflow_artifacts)?;
        }
        validate_turn_suffix(&messages, outcome)?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        // The terminal publication may append raw output, but it may not
        // smuggle in a workflow state that was never checkpointed and ACKed.
        if record.workflow != workflow {
            return Err(StoreError::RunConflict);
        }
        if record.history_gap.is_some() && outcome == Observation::Succeeded {
            return Err(StoreError::RunConflict);
        }
        if record.run.state.is_terminal() {
            if !record
                .operations
                .iter()
                .any(|o| &o.operation_id == operation && o.result_id.as_ref() == Some(&result))
            {
                return Err(StoreError::RunConflict);
            }
            let same_body = same_messages(
                &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                &messages,
            )?;
            return if record.run.state == outcome.state()
                && record.result_id.as_ref() == Some(&result)
                && same_body
                && record.workflow == workflow
            {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let op = record
            .operations
            .iter()
            .position(|o| &o.operation_id == operation)
            .ok_or(StoreError::NotFound)?;
        if (record
            .operations
            .iter()
            .enumerate()
            .any(|(i, o)| i != op && o.result_id.is_none())
            && outcome != Observation::OutcomeUnknown)
            || record.operations[op]
                .result_id
                .as_ref()
                .is_some_and(|r| r != &result)
        {
            return Err(StoreError::RunConflict);
        }
        let state = record
            .run
            .state
            .transition(RunInput::Observed(outcome))
            .map_err(|_| StoreError::RunConflict)?
            .state();
        let mut next = self.published.clone();
        super::children::seal(&mut next.state, target, outcome)?;
        let tail = turn_suffix_tail(&next, next.raw_run_index(index)?, &messages)?;
        let raw_index = next.raw_run_index(index)?;
        append_turn_suffix(&mut next.raw, raw_index, tail)?;
        let record = &mut next.state.runs[index];
        record.operations[op].result_id = Some(result.clone());
        record.run.state = state;
        record.result_id = Some(result);
        record.workflow = workflow.clone();
        next.state.workflow = workflow;
        next.state
            .unresolved_approvals
            .retain(|a| a.run_id != target.run_id || a.attempt_id != target.attempt_id);
        invalidate_approvals(&mut next.state, |a| {
            a.run_id == target.run_id && a.attempt_id == target.attempt_id
        });
        self.commit(next, true)
    }

    /// pendingの耐久保存が完了した場合だけ通知可能な値を返す。
    pub fn publish_approval(
        &mut self,
        approval: PendingApproval,
        now_ms: u64,
    ) -> StoreResult<PendingApproval> {
        self.ready()?;
        let target = RunTarget {
            run_id: approval.run_id.clone(),
            attempt_id: approval.attempt_id.clone(),
        };
        self.check_approval(&approval, &target, now_ms)?;
        if !valid_hash(&approval.payload_hash)
            || approval.operation.is_empty()
            || self.published.state.approval_records.iter().any(|r| {
                r.pending.approval_id == approval.approval_id
                    || r.pending.operation_id == approval.operation_id
            })
            || self.published.state.unresolved_approvals.iter().any(|a| {
                a.approval_id == approval.approval_id || a.operation_id == approval.operation_id
            })
            || self.published.state.runs.iter().any(|r| {
                r.operations
                    .iter()
                    .any(|o| o.operation_id == approval.operation_id)
            })
        {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.unresolved_approvals.push(approval.clone());
        next.state.approval_records.push(ApprovalRecord {
            pending: approval.clone(),
            decision: None,
            consumed: false,
            invalidated: false,
        });
        self.commit(next, false)?;
        Ok(approval)
    }

    fn check_approval(
        &self,
        a: &PendingApproval,
        target: &RunTarget,
        now_ms: u64,
    ) -> StoreResult<()> {
        let index = self.run_index(target)?;
        if a.run_id != target.run_id
            || a.attempt_id != target.attempt_id
            || !self.published.state.runs[index].run.state.permits_start()
            || now_ms >= a.expires_at_unix_ms.get()
        {
            return Err(StoreError::RunConflict);
        }
        cas(
            self.published.state.runs[index].policy_revision,
            self.published.state.policy_revision,
            "run policy",
        )?;
        cas(
            a.policy_revision,
            self.published.state.policy_revision,
            "policy",
        )
    }

    /// now_msはservice所有者の時計。IPC要求から取得してはならない。
    pub fn resolve_approval(&mut self, request: &Request, now_ms: u64) -> StoreResult<Acceptance> {
        self.ready()?;
        let RequestBody::ApprovalResolve(session, params) = &request.body else {
            return Err(StoreError::UnsupportedMethod);
        };
        if session != &self.published.marker.session_id {
            return Err(StoreError::TargetMismatch);
        }
        let hash = content_hash(&serde_json::to_vec(request)?);
        if let Some(existing) =
            self.request_status(session, &request.client_id, &request.request_id)?
        {
            if existing.record.request_hash != hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let index = self
            .published
            .state
            .approval_records
            .iter()
            .position(|r| r.pending.approval_id == params.approval_id)
            .ok_or(StoreError::NotFound)?;
        let ledger = &self.published.state.approval_records[index];
        let a = &ledger.pending;
        if ledger.invalidated
            || ledger.decision.is_some()
            || !self.published.state.unresolved_approvals.contains(a)
            || !a.display.choices.contains(&params.decision)
        {
            return Err(StoreError::RunConflict);
        }
        cas(params.policy_revision, a.policy_revision, "approval policy")?;
        self.check_approval(
            a,
            &RunTarget {
                run_id: params.run_id.clone(),
                attempt_id: params.attempt_id.clone(),
            },
            now_ms,
        )?;
        let mut next = self.published.clone();
        next.state.approval_records[index].decision = Some(params.decision);
        next.state
            .unresolved_approvals
            .retain(|a| a.approval_id != params.approval_id);
        let record = RequestRecord {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            accepted_revision: next.marker.session_revision.checked_add(1)?,
            result: RequestResult::ApprovalResolved {
                approval_id: params.approval_id.clone(),
            },
        };
        next.state.requests.push(record.clone());
        self.commit(next, false)?;
        Ok(self.acceptance(&record))
    }

    /// 許可消費とintentを同じmarkerで公開する。slot登録・draining照合は所有者が直列化する。
    #[allow(clippy::too_many_arguments)]
    pub fn consume_approval_intent(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        operation: &OperationId,
        operation_kind: &str,
        scope: &str,
        payload_hash: &str,
        now_ms: u64,
    ) -> StoreResult<IntentReceipt> {
        let (next, receipt) = self.prepare_approval_intent(
            target,
            approval,
            operation,
            operation_kind,
            scope,
            payload_hash,
            now_ms,
        )?;
        if receipt == IntentReceipt::NewlyPublished {
            self.commit(next, false)?;
        }
        Ok(receipt)
    }

    /// 許可消費とintentを同じmarkerで公開する。slot登録・draining照合は所有者が直列化する。
    #[allow(clippy::too_many_arguments)]
    fn prepare_approval_intent(
        &self,
        target: &RunTarget,
        approval: &ApprovalId,
        operation: &OperationId,
        operation_kind: &str,
        scope: &str,
        payload_hash: &str,
        now_ms: u64,
    ) -> StoreResult<(Published, IntentReceipt)> {
        self.ready()?;
        let index = self
            .published
            .state
            .approval_records
            .iter()
            .position(|r| &r.pending.approval_id == approval)
            .ok_or(StoreError::NotFound)?;
        let ledger = &self.published.state.approval_records[index];
        let a = &ledger.pending;
        self.check_approval(a, target, now_ms)?;
        if ledger.invalidated
            || ledger.decision != Some(ApprovalDecision::Allow)
            || &a.operation_id != operation
            || a.operation != operation_kind
            || a.scope != scope
            || a.payload_hash != payload_hash
        {
            return Err(StoreError::RunConflict);
        }
        if ledger.consumed {
            return Ok((self.published.clone(), IntentReceipt::AlreadyRecorded));
        }
        if self
            .published
            .state
            .runs
            .iter()
            .any(|r| r.operations.iter().any(|o| &o.operation_id == operation))
        {
            return Err(StoreError::RunConflict);
        }
        let run_index = self.run_index(target)?;
        let mut next = self.published.clone();
        next.state.approval_records[index].consumed = true;
        let run = &mut next.state.runs[run_index];
        if run.run.state != RunState::Running {
            run.run.state = run
                .run
                .state
                .transition(RunInput::Control(Control::Start))
                .map_err(|_| StoreError::RunConflict)?
                .state();
        }
        run.operations.push(Operation {
            operation_id: operation.clone(),
            result_id: None,
            mcp: None,
            prompt: None,
            recipe: None,
        });
        Ok((next, IntentReceipt::NewlyPublished))
    }

    pub fn invalidate_approval(&mut self, approval: &ApprovalId) -> StoreResult<()> {
        self.ready()?;
        if !self
            .published
            .state
            .approval_records
            .iter()
            .any(|r| &r.pending.approval_id == approval)
        {
            return Err(StoreError::NotFound);
        }
        let mut next = self.published.clone();
        if invalidate_approvals(&mut next.state, |a| &a.approval_id == approval) {
            self.commit(next, false)?;
        }
        Ok(())
    }

    pub fn expire_approvals(&mut self, now_ms: u64) -> StoreResult<Vec<ApprovalId>> {
        self.ready()?;
        let ids = self
            .published
            .state
            .approval_records
            .iter()
            .filter(|r| !r.invalidated && now_ms >= r.pending.expires_at_unix_ms.get())
            .map(|r| r.pending.approval_id.clone())
            .collect();
        let mut next = self.published.clone();
        if invalidate_approvals(&mut next.state, |a| now_ms >= a.expires_at_unix_ms.get()) {
            self.commit(next, false)?;
        }
        Ok(ids)
    }

    /// Register only after the controller has joined workers, completed cleanup,
    /// synced a private recovery parent and validated its pinned directory FD.
    pub fn publish_source_apply(
        &mut self,
        target: &RunTarget,
        candidate: SourceApplyCandidate,
        guard: SourceApplyGuard,
        proof: SourceApplyIdentityProof,
    ) -> StoreResult<()> {
        self.check_source_apply_guard(target, guard)?;
        if candidate.run_id != target.run_id || candidate.attempt_id != target.attempt_id {
            return Err(StoreError::TargetMismatch);
        }
        candidate.payload.check_proof(proof)?;
        if candidate.policy_revision != guard.expected_policy_revision
            || guard.now_ms >= candidate.expires_at_unix_ms.get()
            || candidate.payload.payload_hash()? != candidate.payload_hash
        {
            return Err(StoreError::RunConflict);
        }
        if let Some(old) = self.published.state.source_applies.iter().find(|r| {
            r.candidate.approval_id == candidate.approval_id
                || r.candidate.operation_id == candidate.operation_id
        }) {
            return if old.candidate == candidate {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies.push(SavedSourceApply {
            candidate,
            decision: None,
            invalidated: false,
            intent_revision: None,
            reserved_result_id: None,
            result: None,
        });
        self.commit(next, false)
    }

    /// Persist a source decision and its request receipt in one publication.
    /// Replayed receipts are evidence only and never grant dispatch authority.
    pub fn resolve_source_apply_request(
        &mut self,
        request: &Request,
        now_ms: u64,
    ) -> StoreResult<Acceptance> {
        self.ready()?;
        let RequestBody::SourceApplyResolve(session, params) = &request.body else {
            return Err(StoreError::UnsupportedMethod);
        };
        if session != &self.published.marker.session_id {
            return Err(StoreError::TargetMismatch);
        }
        let hash = content_hash(&serde_json::to_vec(request)?);
        if let Some(existing) =
            self.request_status(session, &request.client_id, &request.request_id)?
        {
            if existing.record.request_hash != hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let target = RunTarget {
            run_id: params.run_id.clone(),
            attempt_id: params.attempt_id.clone(),
        };
        let guard = SourceApplyGuard {
            expected_session_revision: params.expected_session_revision,
            expected_policy_revision: params.expected_policy_revision,
            now_ms,
        };
        self.check_source_apply_guard(&target, guard)?;
        let index = self.source_apply_index(&target, &params.approval_id)?;
        let record = &self.published.state.source_applies[index];
        Self::check_source_apply_permission(record, guard)?;
        if record.candidate.payload_hash != params.payload_hash || record.decision.is_some() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].decision = Some(params.decision);
        let record = RequestRecord {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            accepted_revision: next.marker.session_revision.checked_add(1)?,
            result: RequestResult::SourceApplyResolved {
                approval_id: params.approval_id.clone(),
            },
        };
        next.state.requests.push(record.clone());
        self.commit(next, false)?;
        Ok(self.acceptance(&record))
    }

    pub fn resolve_source_apply(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        decision: ApprovalDecision,
        guard: SourceApplyGuard,
    ) -> StoreResult<()> {
        self.check_source_apply_guard(target, guard)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        Self::check_source_apply_permission(record, guard)?;
        if let Some(old) = record.decision {
            return if old == decision {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].decision = Some(decision);
        self.commit(next, false)
    }

    /// Durable intent is the permission linearization point. The controller must
    /// serialize this call with cancel/policy changes and validate pinned FDs.
    /// The proof is only compared to stored identities, not OS-verified here.
    /// Only NewlyPublished may be dispatched; errors/recovery/replays never may.
    pub fn consume_source_apply_intent(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        payload_hash: &str,
        guard: SourceApplyGuard,
        proof: SourceApplyIdentityProof,
    ) -> StoreResult<IntentReceipt> {
        self.check_source_apply_guard(target, guard)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        record.candidate.payload.check_proof(proof)?;
        if record.candidate.payload_hash != payload_hash {
            return Err(StoreError::RunConflict);
        }
        if record.intent_revision.is_some() {
            return Ok(IntentReceipt::AlreadyRecorded);
        }
        Self::check_source_apply_permission(record, guard)?;
        if record.decision != Some(ApprovalDecision::Allow) {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].intent_revision =
            Some(next.marker.session_revision.checked_add(1)?);
        self.commit(next, false)?;
        Ok(IntentReceipt::NewlyPublished)
    }

    /// Computes the complete consume row and marker before either management
    /// Prepared publication. Identity proofs remain controller assertions; F5
    /// binds this token to pinned source FDs and the immutable capacity plan.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn precommit_source_apply_m2a(
        &self,
        target: &RunTarget,
        approval: &ApprovalId,
        payload_hash: &str,
        (guard, proof): (SourceApplyGuard, SourceApplyIdentityProof),
        project_binding: crate::management_effect::ManagementEffectHashV1,
        (reserved_result_id, result_capacity_bytes): (ResultId, u64),
    ) -> StoreResult<SourceApplyPrecommitTokenV1> {
        use crate::{
            apply_intent::{d27_operation_binding, d27_result_id_digest},
            management_effect::ManagementEffectHashV1 as Hash,
        };
        use std::os::unix::fs::MetadataExt;
        self.check_source_apply_guard(target, guard)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        record.candidate.payload.check_proof(proof)?;
        Self::check_source_apply_permission(record, guard)?;
        if record.candidate.payload_hash != payload_hash
            || record.decision != Some(ApprovalDecision::Allow)
            || record.intent_revision.is_some()
            || record.reserved_result_id.is_some()
            || record.result.is_some()
        {
            return Err(StoreError::RunConflict);
        }
        if result_capacity_bytes == 0 || result_capacity_bytes > MAX_SOURCE_APPLY_BYTES as u64 {
            return Err(StoreError::Capacity("D27 result reservation"));
        }
        let root_identity = self.root.capacity_identity_m2a()?;
        let logical_capacity = DesktopLogicalCapacityV1 {
            records_remaining: (MAX_SOURCE_APPLY_RECORDS as u64)
                .checked_sub(self.published.state.source_applies.len() as u64)
                .ok_or(StoreError::Overflow)?,
            bytes_remaining: remaining_ledger_bytes_m2a(&self.published.state)?,
        };
        let session_identity = self.directory.identity()?;
        let lock = self.guard.metadata()?;
        let lock_identity = (lock.dev(), lock.ino());
        let mut next = self.published.clone();
        next.state.source_applies[index].intent_revision =
            Some(next.marker.session_revision.checked_add(1)?);
        next.state.source_applies[index].reserved_result_id = Some(reserved_result_id.clone());
        remaining_ledger_bytes_m2a(&next.state)?;
        let next = self.prepare_commit(next, false)?;
        let prediction = disk::predict_commit_m2a(&self.directory, &self.published, &next)?;
        let mut record_bytes = b"polaris-d27-consume-record-v1\0".to_vec();
        record_bytes.extend(serde_json::to_vec(&next.state.source_applies[index])?);
        let precommit = SourceApplyPrecommitV1 {
            schema_version: 1,
            project_binding,
            run_id: target.run_id.clone(),
            attempt_id: target.attempt_id.clone(),
            approval_id: approval.clone(),
            source_operation_digest: d27_operation_binding(record.candidate.operation_id.as_str())
                .map_err(|_| StoreError::Corrupt("source operation binding"))?,
            payload_hash: Hash::from_hex(payload_hash)
                .map_err(|_| StoreError::Corrupt("source payload hash"))?,
            policy_revision: record.candidate.policy_revision.get(),
            base_session_revision: self.published.marker.session_revision.get(),
            consume_target_revision: next.marker.session_revision.get(),
            reserved_result_id_digest: d27_result_id_digest(reserved_result_id.as_str())
                .map_err(|_| StoreError::Corrupt("reserved result ID digest"))?,
            reserved_result_id,
            base_marker_evidence_hash: marker_evidence_hash(
                &prediction.base_marker_bytes,
                &prediction.base_sidecar_prefix,
            ),
            consume_marker_evidence_hash: marker_evidence_hash(
                &prediction.target_marker_bytes,
                &prediction.target_sidecar_prefix,
            ),
            consume_sidecar_offset: prediction.target_marker.sidecar_offset.get(),
            consume_sidecar_hash: Hash::from_hex(&prediction.target_marker.sidecar_hash)
                .map_err(|_| StoreError::Corrupt("consume sidecar hash"))?,
            consume_record_hash: Hash::digest(&record_bytes),
            result_capacity_bytes,
        };
        precommit.canonical_bytes()?;
        self.ready()?;
        if self.root.capacity_identity_m2a()? != root_identity
            || self.directory.identity()? != session_identity
        {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(SourceApplyPrecommitTokenV1 {
            precommit,
            next,
            prediction,
            root_identity,
            session_identity,
            lock_identity,
            logical_capacity,
        })
    }

    /// The shared owner calls this only after both Prepared records are durable.
    /// A successful receipt binds one consume; it is never replay authorization.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn consume_source_apply_precommitted_m2a(
        &mut self,
        token: SourceApplyPrecommitTokenV1,
        guard: SourceApplyGuard,
        proof: SourceApplyIdentityProof,
    ) -> StoreResult<SourceApplyConsumedReceiptV1> {
        let target = RunTarget {
            run_id: token.precommit.run_id.clone(),
            attempt_id: token.precommit.attempt_id.clone(),
        };
        let rebuilt = self.precommit_source_apply_m2a(
            &target,
            &token.precommit.approval_id,
            &token.precommit.payload_hash.as_hex(),
            (guard, proof),
            token.precommit.project_binding,
            (
                token.precommit.reserved_result_id.clone(),
                token.precommit.result_capacity_bytes,
            ),
        )?;
        if rebuilt.precommit != token.precommit
            || rebuilt.prediction != token.prediction
            || rebuilt.root_identity != token.root_identity
            || rebuilt.session_identity != token.session_identity
            || rebuilt.lock_identity != token.lock_identity
        {
            return Err(StoreError::RecoveryRequired);
        }
        self.recovery_required = true;
        let mut next = token.next;
        next.marker = disk::commit_precommitted_m2a(
            &self.directory,
            &self.published,
            &next,
            &token.prediction,
        )?;
        self.root.directory.verify()?;
        self.directory.verify()?;
        let marker_bytes =
            disk::read_exact_m2a(&self.directory, disk::MARKER, MAX_D27_MARKER_BYTES as u64)?;
        let sidecar = disk::read_exact_m2a(
            &self.directory,
            disk::SIDECAR,
            token.precommit.consume_sidecar_offset,
        )?;
        if marker_bytes != token.prediction.target_marker_bytes
            || marker_evidence_hash(&marker_bytes, &sidecar)
                != token.precommit.consume_marker_evidence_hash
        {
            return Err(StoreError::RecoveryRequired);
        }
        self.published = next;
        self.recovery_required = false;
        Ok(SourceApplyConsumedReceiptV1 {
            precommit: token.precommit,
            marker_bytes,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn validate_d27_descriptor_m2a(
        &self,
        precommit: &SourceApplyPrecommitV1,
        descriptor: &crate::apply_intent::ApplyIntentDescriptorV1,
        record: &SavedSourceApply,
        result_id: &ResultId,
    ) -> StoreResult<()> {
        use crate::apply_intent::{
            ApplyContextV1, ApplyInventoryV1, IdentityV1, d27_operation_binding,
            d27_session_binding,
        };
        let source = record.candidate.payload.source_identity;
        let recovery = record.candidate.payload.recovery_parent_identity;
        let identity = self.root.capacity_identity_m2a()?;
        let ApplyContextV1::Session {
            base_epoch,
            session_binding,
            base_session_revision,
            consume_target_revision,
            policy_revision,
            source_operation_digest,
            reserved_result_id,
            reserved_result_id_digest,
            base_marker_evidence_hash,
            consume_marker_evidence_hash,
            consume_sidecar_offset,
            consume_sidecar_hash,
            precommit_hash,
        } = &descriptor.context
        else {
            return Err(StoreError::TargetMismatch);
        };
        if descriptor.inventory_id != ApplyInventoryV1::D27
            || descriptor.project_binding != precommit.project_binding
            || descriptor.source_identity
                != (IdentityV1 {
                    device: source.device.get(),
                    inode: source.inode.get(),
                })
            || descriptor.recovery_parent_identity
                != (IdentityV1 {
                    device: recovery.device.get(),
                    inode: recovery.inode.get(),
                })
            || descriptor.agreed_revision.value != precommit.payload_hash.as_hex()
            || descriptor.capacity_plan.desktop_root_identity != Some(identity.root_identity)
            || descriptor.capacity_plan.desktop_device != Some(identity.root_identity.device)
            || descriptor.capacity_plan.desktop_allocation_unit != Some(identity.allocation_unit)
            || descriptor.capacity_plan.desktop_result_required_bytes
                != Some(precommit.result_capacity_bytes)
            || *base_epoch != self.published.marker.epoch.get()
            || *session_binding
                != d27_session_binding(self.published.marker.session_id.as_str())
                    .map_err(|_| StoreError::TargetMismatch)?
            || *base_session_revision != precommit.base_session_revision
            || *consume_target_revision != precommit.consume_target_revision
            || *policy_revision != precommit.policy_revision
            || *source_operation_digest != precommit.source_operation_digest
            || reserved_result_id != precommit.reserved_result_id.as_str()
            || *reserved_result_id_digest != precommit.reserved_result_id_digest
            || *base_marker_evidence_hash != precommit.base_marker_evidence_hash
            || *consume_marker_evidence_hash != precommit.consume_marker_evidence_hash
            || *consume_sidecar_offset != precommit.consume_sidecar_offset
            || *consume_sidecar_hash != precommit.consume_sidecar_hash
            || *precommit_hash != precommit.precommit_hash()?
            || record.candidate.payload_hash != precommit.payload_hash.as_hex()
            || record.candidate.payload.payload_hash()? != record.candidate.payload_hash
            || record.candidate.policy_revision.get() != precommit.policy_revision
            || d27_operation_binding(record.candidate.operation_id.as_str())
                .map_err(|_| StoreError::TargetMismatch)?
                != precommit.source_operation_digest
            || record.intent_revision.map(DecimalU64::get)
                != Some(precommit.consume_target_revision)
            || record.reserved_result_id.as_ref() != Some(&precommit.reserved_result_id)
            || result_id != &precommit.reserved_result_id
        {
            return Err(StoreError::TargetMismatch);
        }
        Ok(())
    }

    /// Records the exact retained ApplyReport artifact after the shared owner
    /// has joined its hash to ApplyReported. This does not certify apply success.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn source_apply_result_remaining_bytes_m2a(
        &self,
        precommit: &SourceApplyPrecommitV1,
        report_bytes: &[u8],
    ) -> StoreResult<u64> {
        self.ready()?;
        let target = RunTarget {
            run_id: precommit.run_id.clone(),
            attempt_id: precommit.attempt_id.clone(),
        };
        let index = self.source_apply_index(&target, &precommit.approval_id)?;
        let report: crate::workspace_apply::ApplyReport = serde_json::from_slice(report_bytes)?;
        let result = SourceApplyResult {
            result_id: precommit.reserved_result_id.clone(),
            report: serde_json::to_value(report)?,
        };
        if let Some(old) = &self.published.state.source_applies[index].result {
            return if old == &result {
                Ok(0)
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].result = Some(result);
        let next = self.prepare_commit(next, false)?;
        let prediction = disk::predict_commit_m2a(&self.directory, &self.published, &next)?;
        publication_remaining_bytes_m2a(
            &prediction,
            &prediction.base_marker_bytes,
            &prediction.base_sidecar_prefix,
            0,
            self.root.capacity_identity_m2a()?.allocation_unit,
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn record_source_apply_result_evidence_m2a(
        &mut self,
        precommit: &SourceApplyPrecommitV1,
        descriptor: &crate::apply_intent::ApplyIntentDescriptorV1,
        result_id: ResultId,
        report_bytes: &[u8],
        report_hash: crate::management_effect::ManagementEffectHashV1,
    ) -> StoreResult<SourceApplyResultEvidenceV1> {
        use crate::apply_intent::CanonicalApplyEvidence;
        use crate::management_effect::ManagementEffectHashV1 as Hash;
        self.ready()?;
        let identity = self.root.capacity_identity_m2a()?;
        precommit.canonical_bytes()?;
        descriptor
            .canonical_bytes()
            .map_err(|_| StoreError::Corrupt("D27 result descriptor"))?;
        let target = RunTarget {
            run_id: precommit.run_id.clone(),
            attempt_id: precommit.attempt_id.clone(),
        };
        let index = self.source_apply_index(&target, &precommit.approval_id)?;
        let record = &self.published.state.source_applies[index];
        self.validate_d27_descriptor_m2a(precommit, descriptor, record, &result_id)?;
        let mut consumed_record = record.clone();
        consumed_record.result = None;
        let mut consumed_bytes = b"polaris-d27-consume-record-v1\0".to_vec();
        consumed_bytes.extend(serde_json::to_vec(&consumed_record)?);
        if Hash::digest(&consumed_bytes) != precommit.consume_record_hash {
            return Err(StoreError::RecoveryRequired);
        }
        if report_bytes.len() as u64 > crate::workspace_apply::MAX_APPLY_REPORT_BYTES
            || report_bytes.len() as u64 > precommit.result_capacity_bytes
        {
            return Err(StoreError::Capacity("D27 report artifact"));
        }
        let report: crate::workspace_apply::ApplyReport = serde_json::from_slice(report_bytes)?;
        if report
            .canonical_bytes()
            .map_err(|_| StoreError::Corrupt("D27 report encoding"))?
            != report_bytes
        {
            return Err(StoreError::Corrupt("noncanonical D27 report artifact"));
        }
        let mut exact = b"polaris-workspace-apply-report-v1\0".to_vec();
        exact.extend(report_bytes);
        if Hash::digest(&exact) != report_hash
            || report.source_path != std::path::Path::new(&record.candidate.payload.source_path)
        {
            return Err(StoreError::TargetMismatch);
        }
        let result = SourceApplyResult {
            result_id,
            report: serde_json::to_value(report)?,
        };
        let result_length = serde_json::to_vec(&result)?.len() as u64;
        if result_length > precommit.result_capacity_bytes
            || result_length > MAX_SOURCE_APPLY_BYTES as u64
        {
            return Err(StoreError::Capacity("D27 result evidence"));
        }
        result.validate()?;
        let revision = self.published.marker.session_revision.get();
        if revision < precommit.consume_target_revision {
            return Err(StoreError::RecoveryRequired);
        }
        let marker_bytes =
            disk::read_exact_m2a(&self.directory, disk::MARKER, MAX_D27_MARKER_BYTES as u64)?;
        let sidecar = disk::read_exact_m2a(
            &self.directory,
            disk::SIDECAR,
            self.published.marker.sidecar_offset.get(),
        )?;
        let raw = disk::read_exact_m2a(
            &self.directory,
            disk::RAW,
            self.published.marker.raw_offset.get(),
        )?;
        disk::require_published_exact_m2a(
            &self.directory,
            &self.published,
            &marker_bytes,
            &raw,
            &sidecar,
        )?;
        let consume_length =
            usize::try_from(precommit.consume_sidecar_offset).map_err(|_| StoreError::Overflow)?;
        let consume_prefix = sidecar
            .get(..consume_length)
            .ok_or(StoreError::RecoveryRequired)?;
        if content_hash(consume_prefix) != precommit.consume_sidecar_hash.as_hex() {
            return Err(StoreError::RecoveryRequired);
        }
        let inspected = disk::inspect_published_m2a(
            &self.directory,
            &self.published.marker.project_id,
            &self.published.marker.session_id,
        )?;
        let (_, consume_marker, _, _) =
            disk::historical_published_m2a(&inspected, precommit.consume_target_revision)?;
        if marker_evidence_hash(&consume_marker, consume_prefix)
            != precommit.consume_marker_evidence_hash
        {
            return Err(StoreError::RecoveryRequired);
        }
        if let Some(old) = &record.result {
            if old != &result {
                return Err(StoreError::RunConflict);
            }
            // Replay certifies the result publication marker only. A later
            // policy/session commit cannot become new successful evidence.
            let previous_revision = revision
                .checked_sub(1)
                .ok_or(StoreError::RecoveryRequired)?;
            let (previous, _, _, _) =
                disk::historical_published_m2a(&inspected, previous_revision)?;
            if previous.state.source_applies.get(index) != Some(&consumed_record) {
                return Err(StoreError::RecoveryRequired);
            }
            // An explicit joined replay may establish durability, never dispatch.
            self.recovery_required = true;
            disk::sync_published(&self.directory)?;
        } else {
            let mut next = self.published.clone();
            next.state.source_applies[index].result = Some(result);
            let mut next = self.prepare_commit(next, false)?;
            let prediction = disk::predict_commit_m2a(&self.directory, &self.published, &next)?;
            let (_, _, _, base_sidecar) =
                disk::historical_published_m2a(&inspected, precommit.base_session_revision)?;
            require_d27_result_capacity_m2a(
                descriptor,
                consume_length
                    .checked_sub(base_sidecar.len())
                    .ok_or(StoreError::Overflow)?,
                prediction.target_sidecar_prefix.len() - prediction.base_sidecar_prefix.len(),
            )?;
            self.recovery_required = true;
            next.marker = disk::commit_precommitted_m2a(
                &self.directory,
                &self.published,
                &next,
                &prediction,
            )?;
            self.published = next;
        }
        self.root.directory.verify()?;
        if self.root.capacity_identity_m2a()? != identity {
            self.recovery_required = true;
            return Err(StoreError::RecoveryRequired);
        }
        let marker_bytes =
            disk::read_exact_m2a(&self.directory, disk::MARKER, MAX_D27_MARKER_BYTES as u64)?;
        let sidecar = disk::read_exact_m2a(
            &self.directory,
            disk::SIDECAR,
            self.published.marker.sidecar_offset.get(),
        )?;
        let raw = disk::read_exact_m2a(
            &self.directory,
            disk::RAW,
            self.published.marker.raw_offset.get(),
        )?;
        disk::require_published_exact_m2a(
            &self.directory,
            &self.published,
            &marker_bytes,
            &raw,
            &sidecar,
        )?;
        self.recovery_required = false;
        Ok(SourceApplyResultEvidenceV1 {
            session_revision: self.published.marker.session_revision.get(),
            marker_evidence_hash: marker_evidence_hash(&marker_bytes, &sidecar),
            marker_bytes,
            sidecar_offset: self.published.marker.sidecar_offset.get(),
            sidecar_hash: Hash::from_hex(&self.published.marker.sidecar_hash)
                .map_err(|_| StoreError::RecoveryRequired)?,
            sidecar_prefix_hash: Hash::digest(&sidecar),
            reserved_result_id_digest: precommit.reserved_result_id_digest,
            report_hash,
        })
    }

    /// Expiry or later revocation cannot suppress evidence of an in-flight apply.
    pub fn record_source_apply_result(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        expected_session_revision: DecimalU64,
        result: SourceApplyResult,
    ) -> StoreResult<()> {
        self.check_source_apply_target(target, expected_session_revision)?;
        let index = self
            .published
            .state
            .source_applies
            .iter()
            .position(|r| {
                &r.candidate.operation_id == operation
                    && r.candidate.run_id == target.run_id
                    && r.candidate.attempt_id == target.attempt_id
            })
            .ok_or(StoreError::NotFound)?;
        let record = &self.published.state.source_applies[index];
        if record.intent_revision.is_none() {
            return Err(StoreError::RunConflict);
        }
        result.validate()?;
        if let Some(old) = &record.result {
            return if old == &result {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].result = Some(result);
        self.commit(next, false)
    }

    /// Invalidates future permission only; never erases or cancels an intent.
    pub fn invalidate_source_apply(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        expected_session_revision: DecimalU64,
    ) -> StoreResult<()> {
        self.check_source_apply_target(target, expected_session_revision)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        if record.invalidated || record.intent_revision.is_some() {
            return Ok(());
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].invalidated = true;
        self.commit(next, false)
    }

    fn check_source_apply_target(
        &self,
        target: &RunTarget,
        expected: DecimalU64,
    ) -> StoreResult<()> {
        self.ready()?;
        cas(
            expected,
            self.published.marker.session_revision,
            "source apply session",
        )?;
        let run = &self.published.state.runs[self.run_index(target)?];
        if !run.run.state.is_terminal() || run.result_id.is_none() {
            return Err(StoreError::RunConflict);
        }
        Ok(())
    }

    fn check_source_apply_guard(
        &self,
        target: &RunTarget,
        guard: SourceApplyGuard,
    ) -> StoreResult<()> {
        self.check_source_apply_target(target, guard.expected_session_revision)?;
        cas(
            guard.expected_policy_revision,
            self.published.state.policy_revision,
            "source apply policy",
        )
    }

    fn source_apply_index(&self, target: &RunTarget, approval: &ApprovalId) -> StoreResult<usize> {
        self.published
            .state
            .source_applies
            .iter()
            .position(|r| {
                &r.candidate.approval_id == approval
                    && r.candidate.run_id == target.run_id
                    && r.candidate.attempt_id == target.attempt_id
            })
            .ok_or(StoreError::NotFound)
    }

    fn check_source_apply_permission(
        record: &SavedSourceApply,
        guard: SourceApplyGuard,
    ) -> StoreResult<()> {
        if record.invalidated
            || guard.now_ms >= record.candidate.expires_at_unix_ms.get()
            || record.candidate.policy_revision != guard.expected_policy_revision
        {
            return Err(StoreError::RunConflict);
        }
        Ok(())
    }

    pub fn set_policy_revision(&mut self, revision: DecimalU64) -> StoreResult<()> {
        self.ready()?;
        if revision < self.published.state.policy_revision {
            return Err(StoreError::CasConflict("policy"));
        }
        if revision == self.published.state.policy_revision {
            return Ok(());
        }
        let mut next = self.published.clone();
        next.state.policy_revision = revision;
        super::source_apply::invalidate_unconsumed(&mut next.state);
        invalidate_approvals(&mut next.state, |_| true);
        self.commit(next, false)
    }

    fn run_index(&self, target: &RunTarget) -> StoreResult<usize> {
        self.published
            .state
            .runs
            .iter()
            .position(|r| r.run.run_id == target.run_id && r.run.attempt_id == target.attempt_id)
            .ok_or(StoreError::NotFound)
    }

    /// 停止済みsessionのtombstoneを同じ公開点へ保存する。log・台帳は消去しない。
    pub fn tombstone(&mut self) -> StoreResult<()> {
        if !self.recovery_required && self.published.marker.deleted {
            return Ok(());
        }
        self.ready()?;
        if self
            .published
            .state
            .runs
            .iter()
            .any(|r| !r.run.state.is_terminal())
        {
            return Err(StoreError::Busy);
        }
        let mut next = self.published.clone();
        if next
            .state
            .source_applies
            .iter()
            .any(|r| r.intent_revision.is_some() && r.result.is_none())
        {
            return Err(StoreError::Busy);
        }
        super::source_apply::invalidate_unconsumed(&mut next.state);
        next.marker.deleted = true;
        next.state.unresolved_approvals.clear();
        self.commit(next, false)
    }
}

fn cas(expected: DecimalU64, actual: DecimalU64, name: &'static str) -> StoreResult<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(StoreError::CasConflict(name))
    }
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn same_message(left: &Message, right: &Message) -> StoreResult<bool> {
    Ok(serde_json::to_vec(left)? == serde_json::to_vec(right)?)
}

fn same_messages(left: &[Message], right: &[Message]) -> StoreResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    left.iter()
        .zip(right)
        .try_fold(true, |same, (left, right)| {
            Ok(same && same_message(left, right)?)
        })
}

fn saved_turn_suffix(p: &Published, index: usize) -> StoreResult<Vec<Message>> {
    let run_index = index
        .checked_sub(
            usize::try_from(p.state.inherited_turn_count.get())
                .map_err(|_| StoreError::Overflow)?,
        )
        .ok_or(StoreError::RunConflict)?;
    let supplemental = prompt::attachment(p, run_index)?.map(|event| event.sequence);
    let input = p
        .raw
        .iter()
        .filter(|event| event.starts_turn)
        .nth(index)
        .ok_or(StoreError::RunConflict)?;
    Ok(p.raw
        .iter()
        .filter(|event| {
            event.turn_id == input.turn_id
                && !event.starts_turn
                && Some(event.sequence) != supplemental
        })
        .map(|event| event.message.clone())
        .collect())
}

fn turn_suffix_tail(p: &Published, index: usize, full: &[Message]) -> StoreResult<Vec<Message>> {
    let saved = saved_turn_suffix(p, index)?;
    if saved.len() > full.len()
        || !saved
            .iter()
            .zip(full)
            .try_fold(true, |same, (saved, full)| {
                Ok::<bool, StoreError>(same && same_message(saved, full)?)
            })?
    {
        return Err(StoreError::RunConflict);
    }
    Ok(full[saved.len()..].to_vec())
}

fn append_turn_suffix(
    raw: &mut Vec<RawEventV2>,
    index: usize,
    tail: Vec<Message>,
) -> StoreResult<()> {
    if tail.is_empty() {
        return Ok(());
    }
    // Only the current accepted turn may acquire a new suffix.
    if raw.iter().filter(|event| event.starts_turn).count() != index + 1 {
        return Err(StoreError::RunConflict);
    }
    let input = raw
        .iter()
        .filter(|event| event.starts_turn)
        .nth(index)
        .ok_or(StoreError::RunConflict)?;
    let turn_id = input.turn_id;
    let mut sequence = raw.last().ok_or(StoreError::RunConflict)?.sequence;
    let epoch = raw.last().ok_or(StoreError::RunConflict)?.epoch;
    for message in tail {
        sequence = sequence.checked_add(1).ok_or(StoreError::Overflow)?;
        raw.push(RawEventV2 {
            schema_version: 2,
            sequence,
            epoch,
            turn_id,
            starts_turn: false,
            message,
        });
    }
    Ok(())
}

fn same_or_staled_gates(
    previous: &crate::workflow::WorkflowGatesV1,
    candidate: &crate::workflow::WorkflowGatesV1,
) -> bool {
    if previous.schema_version != candidate.schema_version
        || previous.phase_gate_schema_version != candidate.phase_gate_schema_version
        || previous.approved_spec_hash != candidate.approved_spec_hash
        || previous.convergence != candidate.convergence
        || previous.full_spec != candidate.full_spec
        || previous.external_grants != candidate.external_grants
    {
        return false;
    }
    match (&previous.verification, &candidate.verification) {
        (None, None) => true,
        (Some(previous), Some(candidate)) => {
            previous.artifact_hash == candidate.artifact_hash
                && previous.passed == candidate.passed
                && (!previous.stale || candidate.stale)
        }
        _ => false,
    }
}

fn checkpoint_base_matches(saved: &super::SavedWorkflow, previous: &super::SavedWorkflow) -> bool {
    let mut previous_without_first_manifest = previous.state.clone();
    previous_without_first_manifest.skill_manifest_hash.clear();
    previous_without_first_manifest.skills.clear();
    saved.config == previous.config
        && same_or_staled_gates(&saved.gates, &previous.gates)
        && (saved.state == previous.state
            // `begin_turn` fixes the first request's manifest before provider
            // admission. A legacy durable snapshot has no manifest yet; every
            // other field, including step and active artifact, must match.
            || (saved.state == previous_without_first_manifest
                && !previous.state.skill_manifest_hash.is_empty()))
}

pub(crate) fn validate_turn_suffix(messages: &[Message], outcome: Observation) -> StoreResult<()> {
    use polaris_provider::Role;
    use std::collections::BTreeSet;
    // Count serialized bytes without making a second unbounded copy.
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
                std::io::Error::other("desktop turn exceeds storage transaction limit")
            })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if messages.len() > 256 {
        return Err(StoreError::Overflow);
    }
    serde_json::to_writer(Budget(4 * 1024 * 1024), messages).map_err(|_| StoreError::Overflow)?;
    let mut seen = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for message in messages {
        // history.page returns up to four complete text values in a 1 MiB frame.
        // Bound escaped JSON, not UTF-8 alone; never silently truncate raw text.
        serde_json::to_writer(Budget(128 * 1024), &message.content)
            .map_err(|_| StoreError::Overflow)?;
        match message.role {
            Role::User => return Err(StoreError::RunConflict),
            Role::Assistant => {
                if message.tool_call_id.is_some() || !pending.is_empty() {
                    return Err(StoreError::RunConflict);
                }
                for call in &message.tool_calls {
                    if call.id.is_empty() || !seen.insert(call.id.as_str()) {
                        return Err(StoreError::RunConflict);
                    }
                    pending.insert(call.id.as_str());
                }
            }
            Role::Tool => {
                if !message.tool_calls.is_empty()
                    || !message.reasoning.is_empty()
                    || !message.hosted_web_search.is_empty()
                    || !message.url_citations.is_empty()
                    || !message
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| pending.remove(id))
                {
                    return Err(StoreError::RunConflict);
                }
            }
        }
    }
    if outcome == Observation::Succeeded
        && (!pending.is_empty() || !messages.last().is_some_and(|m| m.role == Role::Assistant))
    {
        return Err(StoreError::RunConflict);
    }
    Ok(())
}

fn invalidate_approvals(state: &mut Sidecar, predicate: impl Fn(&PendingApproval) -> bool) -> bool {
    let before = state.unresolved_approvals.len();
    state.unresolved_approvals.retain(|a| !predicate(a));
    let mut changed = before != state.unresolved_approvals.len();
    for record in &mut state.approval_records {
        if !record.invalidated && predicate(&record.pending) {
            record.invalidated = true;
            changed = true;
        }
    }
    changed
}
