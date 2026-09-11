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
        {
            return Err(StoreError::Corrupt("source apply ledger binding"));
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
            || (prior.invalidated
                && prior.intent_revision.is_none()
                && current.intent_revision.is_some())
        {
            return Err(StoreError::Corrupt("source apply evidence rewritten"));
        }
    }
    Ok(())
}
