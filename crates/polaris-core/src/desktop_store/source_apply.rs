//! Controller-only evidence ledger. No filesystem authorization or dispatch.
//! Identity proofs are caller assertions: the controller must validate pinned FDs
//! immediately before intent/apply while serializing policy and cancellation.

use super::{Sidecar, StoreError, StoreResult};
use crate::conversation_state::content_hash;
use polaris_desktop_protocol::{ids::*, snapshot::ApprovalDecision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_SOURCE_APPLY_ENTRIES: usize = 1024;
pub const MAX_SOURCE_APPLY_BYTES: usize = 1024 * 1024;
pub const MAX_SOURCE_APPLY_RECORDS: usize = 1024;
pub const MAX_SOURCE_APPLY_LEDGER_BYTES: usize = 8 * 1024 * 1024;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn source_apply_result_capacity_m2a(
    result_id: &ResultId,
    report_upper_bound: u64,
) -> StoreResult<u64> {
    let wrapper = SourceApplyResult {
        result_id: result_id.clone(),
        report: serde_json::Value::Null,
    };
    let wrapper_bytes = encoded_size_m2a(&wrapper)?;
    wrapper_bytes
        .checked_sub(4)
        .and_then(|n| n.checked_add(report_upper_bound))
        .ok_or(StoreError::Overflow)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn encoded_size_m2a(value: &impl Serialize) -> StoreResult<u64> {
    struct Count(u64);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("desktop evidence length overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn remaining_ledger_bytes_m2a(state: &Sidecar) -> StoreResult<u64> {
    let serialized = encoded_size_m2a(&state.source_applies)?;
    let pending = u64::try_from(
        state
            .source_applies
            .iter()
            .filter(|r| r.intent_revision.is_some() && r.result.is_none())
            .count(),
    )
    .map_err(|_| StoreError::Overflow)?;
    let reserved = pending
        .checked_mul(MAX_SOURCE_APPLY_BYTES as u64)
        .ok_or(StoreError::Overflow)?;
    (MAX_SOURCE_APPLY_LEDGER_BYTES as u64)
        .checked_sub(serialized)
        .and_then(|n| n.checked_sub(reserved))
        .ok_or(StoreError::Capacity("D27 pending result reservation"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod m2a {
    use super::*;
    use crate::{
        apply_intent::d27_result_id_digest, management_effect::ManagementEffectHashV1 as Hash,
    };

    pub const MAX_D27_MARKER_BYTES: usize = 65_536;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SourceApplyPrecommitV1 {
        pub schema_version: u8,
        pub project_binding: Hash,
        pub run_id: RunId,
        pub attempt_id: AttemptId,
        pub approval_id: ApprovalId,
        pub source_operation_digest: Hash,
        pub payload_hash: Hash,
        pub policy_revision: u64,
        pub base_session_revision: u64,
        pub consume_target_revision: u64,
        pub reserved_result_id: ResultId,
        pub reserved_result_id_digest: Hash,
        pub base_marker_evidence_hash: Hash,
        pub consume_marker_evidence_hash: Hash,
        pub consume_sidecar_offset: u64,
        pub consume_sidecar_hash: Hash,
        pub consume_record_hash: Hash,
        pub result_capacity_bytes: u64,
    }

    impl SourceApplyPrecommitV1 {
        pub fn canonical_bytes(&self) -> StoreResult<Vec<u8>> {
            let result_digest = d27_result_id_digest(self.reserved_result_id.as_str())
                .map_err(|_| StoreError::Corrupt("reserved result ID digest"))?;
            if self.schema_version != 1
                || self.base_session_revision.checked_add(1) != Some(self.consume_target_revision)
                || self.reserved_result_id_digest != result_digest
                || self.consume_sidecar_offset == 0
                || self.result_capacity_bytes == 0
                || self.result_capacity_bytes > MAX_SOURCE_APPLY_BYTES as u64
                || [
                    self.project_binding,
                    self.source_operation_digest,
                    self.payload_hash,
                    self.reserved_result_id_digest,
                    self.base_marker_evidence_hash,
                    self.consume_marker_evidence_hash,
                    self.consume_sidecar_hash,
                    self.consume_record_hash,
                ]
                .into_iter()
                .any(|hash| hash.as_hex().bytes().all(|byte| byte == b'0'))
            {
                return Err(StoreError::Corrupt("source apply precommit"));
            }
            Ok(serde_json::to_vec(self)?)
        }

        pub fn from_canonical_bytes(bytes: &[u8]) -> StoreResult<Self> {
            let value: Self = serde_json::from_slice(bytes)?;
            if value.canonical_bytes()? != bytes {
                return Err(StoreError::Corrupt("noncanonical source apply precommit"));
            }
            Ok(value)
        }

        pub fn precommit_hash(&self) -> StoreResult<Hash> {
            let mut bytes = b"polaris-d27-source-apply-precommit-v1\0".to_vec();
            bytes.extend(self.canonical_bytes()?);
            Ok(Hash::digest(&bytes))
        }
    }

    pub(crate) fn marker_evidence_hash(marker: &[u8], sidecar_prefix: &[u8]) -> Hash {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"polaris-source-apply-session-marker-v1\0");
        digest.update((marker.len() as u64).to_be_bytes());
        digest.update(marker);
        digest.update((sidecar_prefix.len() as u64).to_be_bytes());
        digest.update(sidecar_prefix);
        let hex = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Hash::from_hex(&hex).expect("fixed-width SHA-256")
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) use m2a::marker_evidence_hash;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use m2a::{MAX_D27_MARKER_BYTES, SourceApplyPrecommitV1};

/// Caller samples time under the same controller serialization as policy/cancel.
#[derive(Debug, Clone, Copy)]
pub struct SourceApplyGuard {
    pub expected_session_revision: DecimalU64,
    pub expected_policy_revision: DecimalU64,
    pub now_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyIdentity {
    pub device: DecimalU64,
    pub inode: DecimalU64,
}

/// Not a capability. The store compares these values; it never examines an FD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceApplyIdentityProof {
    pub source: SourceApplyIdentity,
    pub recovery_parent: SourceApplyIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyVersion {
    pub hash: String,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyEntry {
    pub relative_path: String,
    pub before: Option<SourceApplyVersion>,
    pub after: Option<SourceApplyVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyPayload {
    pub source_path: String,
    pub source_identity: SourceApplyIdentity,
    pub recovery_parent_path: String,
    pub recovery_parent_identity: SourceApplyIdentity,
    pub entries: Vec<SourceApplyEntry>,
}

impl SourceApplyPayload {
    pub fn validate(&self) -> StoreResult<()> {
        if !absolute(&self.source_path)
            || !absolute(&self.recovery_parent_path)
            || self.source_identity.device != self.recovery_parent_identity.device
            || self.source_identity == self.recovery_parent_identity
            || self.recovery_parent_path == self.source_path
            || self
                .recovery_parent_path
                .starts_with(&(self.source_path.clone() + "/"))
            || self.entries.len() > MAX_SOURCE_APPLY_ENTRIES
        {
            return Err(StoreError::Corrupt("source apply payload"));
        }
        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            if !relative(&entry.relative_path)
                || previous.is_some_and(|p| p >= entry.relative_path.as_str())
                || (entry.before.is_none() && entry.after.is_none())
                || entry.before == entry.after
            {
                return Err(StoreError::Corrupt("source apply entry"));
            }
            for version in [&entry.before, &entry.after].into_iter().flatten() {
                if !hash(&version.hash) || version.mode > 0o7777 {
                    return Err(StoreError::Corrupt("source apply version"));
                }
            }
            previous = Some(&entry.relative_path);
        }
        bounded(self)
    }

    pub fn payload_hash(&self) -> StoreResult<String> {
        self.validate()?;
        Ok(content_hash(&serde_json::to_vec(self)?))
    }

    pub(super) fn check_proof(&self, proof: SourceApplyIdentityProof) -> StoreResult<()> {
        if proof.source != self.source_identity
            || proof.recovery_parent != self.recovery_parent_identity
        {
            return Err(StoreError::TargetMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyCandidate {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub approval_id: ApprovalId,
    pub operation_id: OperationId,
    pub policy_revision: DecimalU64,
    pub expires_at_unix_ms: DecimalU64,
    pub payload: SourceApplyPayload,
    pub payload_hash: String,
}

/// Bounded serialized ApplyReport, retained verbatim as evidence, never executed.
/// The controller owns conversion and report/FD correspondence, including partial
/// syscall effects. A stored report does not certify source-apply success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyResult {
    pub result_id: ResultId,
    pub report: serde_json::Value,
}

impl SourceApplyResult {
    pub(super) fn validate(&self) -> StoreResult<()> {
        bounded(self)?;
        let object = self
            .report
            .as_object()
            .ok_or(StoreError::Corrupt("source apply report"))?;
        let entries = object
            .get("entries")
            .and_then(|v| v.as_array())
            .ok_or(StoreError::Corrupt("source apply report entries"))?;
        if entries.len() > MAX_SOURCE_APPLY_ENTRIES {
            return Err(StoreError::Corrupt("source apply report bounds"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedSourceApply {
    pub candidate: SourceApplyCandidate,
    pub decision: Option<ApprovalDecision>,
    pub invalidated: bool,
    pub intent_revision: Option<DecimalU64>,
    /// Bound in the same commit as an M2-A consume. Legacy service intents
    /// retain None and cannot authorize the new shared owner.
    pub reserved_result_id: Option<ResultId>,
    pub result: Option<SourceApplyResult>,
}

pub(super) fn validate(state: &Sidecar) -> StoreResult<()> {
    if state.source_applies.len() > MAX_SOURCE_APPLY_RECORDS {
        return Err(StoreError::Corrupt("source apply ledger bounds"));
    }
    // Reserve a maximum serialized result for each durable, unreported intent.
    // The current `null` remains counted conservatively. All commits and every
    // historical state use this same check, so later candidates cannot spend it.
    let reserved = state
        .source_applies
        .iter()
        .filter(|r| r.intent_revision.is_some() && r.result.is_none())
        .count()
        .checked_mul(MAX_SOURCE_APPLY_BYTES)
        .ok_or(StoreError::Overflow)?;
    let available = MAX_SOURCE_APPLY_LEDGER_BYTES
        .checked_sub(reserved)
        .ok_or(StoreError::Corrupt("source apply result reservation"))?;
    bounded_to(&state.source_applies, available)?;
    let mut approvals = BTreeSet::new();
    let mut operations = BTreeSet::new();
    let mut results = BTreeSet::new();
    let mut reservations = BTreeSet::new();
    for record in &state.source_applies {
        let c = &record.candidate;
        if !approvals.insert(&c.approval_id)
            || !operations.insert(&c.operation_id)
            || c.payload.payload_hash()? != c.payload_hash
            || c.policy_revision > state.policy_revision
            || (c.policy_revision != state.policy_revision
                && record.intent_revision.is_none()
                && !record.invalidated)
            || !state.runs.iter().any(|r| {
                r.run.run_id == c.run_id
                    && r.run.attempt_id == c.attempt_id
                    && r.run.state.is_terminal()
                    && r.result_id.is_some()
            })
            || state.approval_records.iter().any(|a| {
                a.pending.approval_id == c.approval_id || a.pending.operation_id == c.operation_id
            })
            || state.runs.iter().any(|r| {
                r.operations
                    .iter()
                    .any(|o| o.operation_id == c.operation_id)
            })
            || record
                .intent_revision
                .is_some_and(|r| r.get() == 0 || r > state.session_revision)
            || (record.intent_revision.is_some()
                && record.decision != Some(ApprovalDecision::Allow))
            || (record.result.is_some() && record.intent_revision.is_none())
            || (record.reserved_result_id.is_some() && record.intent_revision.is_none())
        {
            return Err(StoreError::Corrupt("source apply ledger binding"));
        }
        if let Some(reserved) = &record.reserved_result_id {
            if !reservations.insert(reserved)
                || record
                    .result
                    .as_ref()
                    .is_some_and(|result| &result.result_id != reserved)
                || state.runs.iter().any(|run| {
                    run.result_id.as_ref() == Some(reserved)
                        || run
                            .operations
                            .iter()
                            .any(|operation| operation.result_id.as_ref() == Some(reserved))
                })
                || state.source_applies.iter().any(|other| {
                    other.candidate.operation_id != c.operation_id
                        && other
                            .result
                            .as_ref()
                            .is_some_and(|result| &result.result_id == reserved)
                })
            {
                return Err(StoreError::Corrupt(
                    "source apply result reservation collision",
                ));
            }
        }
        if let Some(result) = &record.result {
            result.validate()?;
            if !results.insert(&result.result_id) {
                return Err(StoreError::Corrupt("source apply result duplicate"));
            }
        }
    }
    Ok(())
}

pub(super) fn invalidate_unconsumed(state: &mut Sidecar) -> bool {
    let mut changed = false;
    for record in &mut state.source_applies {
        if record.intent_revision.is_none() && !record.invalidated {
            record.invalidated = true;
            changed = true;
        }
    }
    changed
}

fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\\'))
}
fn absolute(value: &str) -> bool {
    value.strip_prefix('/').is_some_and(relative)
}
fn bounded(value: &impl Serialize) -> StoreResult<()> {
    bounded_to(value, MAX_SOURCE_APPLY_BYTES)
}
fn bounded_to(value: &impl Serialize, limit: usize) -> StoreResult<()> {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("source apply bounds"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(limit), value)?;
    Ok(())
}

pub(super) fn validate_transition(old: &Sidecar, next: &Sidecar) -> StoreResult<()> {
    for current in &next.source_applies {
        let prior = old
            .source_applies
            .iter()
            .find(|r| r.candidate.operation_id == current.candidate.operation_id);
        if prior.is_none()
            && (current.decision.is_some()
                || current.intent_revision.is_some()
                || current.result.is_some())
        {
            return Err(StoreError::Corrupt("source apply skipped registration"));
        }
        if current.intent_revision.is_some()
            && prior.is_some_and(|r| r.intent_revision.is_none())
            && (current.intent_revision != Some(next.session_revision)
                || prior
                    .is_some_and(|r| r.decision != Some(ApprovalDecision::Allow) || r.invalidated))
        {
            return Err(StoreError::Corrupt("source apply skipped approval"));
        }
        if current.reserved_result_id.is_some()
            && prior.is_none_or(|prior| {
                prior.reserved_result_id.is_none()
                    && (prior.intent_revision.is_some()
                        || current.intent_revision != Some(next.session_revision))
            })
        {
            return Err(StoreError::Corrupt(
                "source apply reservation without same-commit consume",
            ));
        }
    }
    for prior in &old.source_applies {
        let current = next
            .source_applies
            .iter()
            .find(|r| r.candidate.operation_id == prior.candidate.operation_id)
            .ok_or(StoreError::Corrupt("source apply evidence removed"))?;
        if current.candidate != prior.candidate
            || (prior.decision.is_some() && current.decision != prior.decision)
            || (prior.invalidated && !current.invalidated)
            || (prior.intent_revision.is_some() && current.intent_revision != prior.intent_revision)
            || (prior.result.is_some() && current.result != prior.result)
            || (prior.reserved_result_id.is_some()
                && current.reserved_result_id != prior.reserved_result_id)
            || (prior.invalidated
                && prior.intent_revision.is_none()
                && current.intent_revision.is_some())
        {
            return Err(StoreError::Corrupt("source apply evidence rewritten"));
        }
    }
    Ok(())
}
