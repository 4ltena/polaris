//! Non-secret, whole-chain evidence for the per-user MCP credential ledger.
//! Parsing is read-only. Missing roots and incomplete tails are never empty ledgers.

// Physical ledger I/O stays disconnected until migration authority is admitted.
#[cfg(test)]
pub(crate) mod file_store;
pub(crate) mod payload_head;

use super::documents_v2::{
    DocumentKindV2, DocumentMetadataV2, DocumentPresenceV2, DocumentReceiptV2,
    ManagedDocumentReceiptV1,
};
use crate::{
    Error,
    entry_gate::EntryAnchorV2,
    registration::{bounded_json, canonical_hash, valid_id},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_LEDGER_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_LEDGER_RECORDS: usize = 100_000;
pub const MAX_LEDGER_OPERATIONS: usize = 20_000;
pub const MAX_LEDGER_RECORD_BYTES: usize = 16 * 1024;
pub const MAX_RECOVERY_DEPTH: u8 = 8;

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn hash(value: &impl Serialize) -> Result<String, Error> {
    canonical_hash(&serde_json::to_value(value).map_err(|_| Error::Protocol)?)
}
fn crate_operation_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn parent_directory(anchor: &EntryAnchorV2) -> bool {
    anchor.inode != 0
        && anchor.uid == unsafe { libc::geteuid() }
        && anchor.mode & 0o7022 == 0
        && anchor.links > 0
}
pub fn operation_reference_hash(references: &[String]) -> Result<String, Error> {
    if references.is_empty()
        || references.len() > 1024
        || references.iter().any(|v| !valid_id(v))
        || references.windows(2).any(|v| v[0] >= v[1])
    {
        return Err(Error::Protocol);
    }
    let mut bytes = b"polaris-mcp-reference-set-v1\0".to_vec();
    bytes.extend_from_slice(&bounded_json(&references, MAX_LEDGER_RECORD_BYTES)?);
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn directory(anchor: &EntryAnchorV2) -> bool {
    anchor.inode != 0
        && anchor.uid == unsafe { libc::geteuid() }
        && anchor.mode == 0o700
        && anchor.links > 0
}
fn regular(anchor: &EntryAnchorV2) -> bool {
    anchor.inode != 0
        && anchor.uid == unsafe { libc::geteuid() }
        && anchor.mode == 0o600
        && anchor.links == 1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TombstoneLedgerAnchorV1 {
    pub anchor: EntryAnchorV2,
    pub file_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredLedgerAnchorV1 {
    pub anchor: EntryAnchorV2,
    pub non_secret_tree_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerEpochV1 {
    pub protocol_version: u32,
    pub store_identity: String,
    pub random_epoch_id: String,
    pub migration_id: String,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub tombstone_anchor: TombstoneLedgerAnchorV1,
    pub retired_source_anchor: RetiredLedgerAnchorV1,
    pub active_v2_root_anchor: EntryAnchorV2,
    pub activation_tree_hash: String,
}
impl LedgerEpochV1 {
    pub fn validate(&self) -> Result<(), Error> {
        if self.protocol_version != 2
            || ![
                &self.store_identity,
                &self.random_epoch_id,
                &self.migration_id,
                &self.activation_tree_hash,
                &self.tombstone_anchor.file_hash,
                &self.retired_source_anchor.non_secret_tree_hash,
            ]
            .into_iter()
            .all(|v| digest(v))
            || !parent_directory(&self.canonical_parent_anchor)
            || !directory(&self.retired_source_anchor.anchor)
            || !directory(&self.active_v2_root_anchor)
            || !regular(&self.tombstone_anchor.anchor)
        {
            return Err(Error::Protocol);
        }
        let anchors = [
            &self.canonical_parent_anchor,
            &self.retired_source_anchor.anchor,
            &self.active_v2_root_anchor,
            &self.tombstone_anchor.anchor,
        ];
        if anchors
            .iter()
            .map(|a| (a.device, a.inode))
            .collect::<BTreeSet<_>>()
            .len()
            != anchors.len()
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        bounded_json(self, MAX_LEDGER_RECORD_BYTES)
    }
    pub fn canonical_hash(&self) -> Result<String, Error> {
        self.validate()?;
        hash(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialLedgerInventoryV1 {
    C24,
    C25,
    C27,
    D37,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialLedgerStepKindV1 {
    Set,
    Delete,
    EnsureAbsent,
    ApplyBatchAdd,
    PruneStale,
    CompensateAdded,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialLedgerStepV1 {
    pub references: Vec<String>,
    pub kind: CredentialLedgerStepKindV1,
    /// Hash of the operation's canonical reference list, not all document references.
    pub operation_reference_set_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialLedgerPlanV1 {
    Single {
        step: CredentialLedgerStepV1,
    },
    Setup {
        add: Option<CredentialLedgerStepV1>,
        prune: Option<CredentialLedgerStepV1>,
        registry_before: DocumentMetadataV2,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialLedgerDescriptorV1 {
    pub references: Vec<String>,
    pub operation_id: String,
    pub origin_project_identity: String,
    pub store_identity: String,
    pub inventory: CredentialLedgerInventoryV1,
    pub root_operation_id: String,
    pub predecessor_operation_id: Option<String>,
    pub chain_depth: u8,
    pub plan_hash: String,
    pub operation_reference_set_hash: String,
    pub authorization_anchor_hash: String,
    pub credential_before: DocumentMetadataV2,
    pub plan: CredentialLedgerPlanV1,
}
impl CredentialLedgerDescriptorV1 {
    fn validate(&self, epoch: &LedgerEpochV1) -> Result<(), Error> {
        self.credential_before.validate()?;
        if !crate_operation_id(&self.operation_id)
            || !crate_operation_id(&self.root_operation_id)
            || self
                .predecessor_operation_id
                .as_ref()
                .is_some_and(|v| !crate_operation_id(v))
            || self.chain_depth == 0
            || self.chain_depth > MAX_RECOVERY_DEPTH
            || self.store_identity != epoch.store_identity
            || self.credential_before.kind != DocumentKindV2::Credential
            || ![
                &self.origin_project_identity,
                &self.plan_hash,
                &self.operation_reference_set_hash,
                &self.authorization_anchor_hash,
            ]
            .into_iter()
            .all(|v| digest(v))
        {
            return Err(Error::Protocol);
        }
        if operation_reference_hash(&self.references)? != self.operation_reference_set_hash {
            return Err(Error::Protocol);
        }
        let steps: Vec<_> = match &self.plan {
            CredentialLedgerPlanV1::Single { step } => vec![step],
            CredentialLedgerPlanV1::Setup { add, prune, .. } => {
                add.iter().chain(prune.iter()).collect()
            }
        };
        let mut union = BTreeSet::new();
        for step in steps {
            if operation_reference_hash(&step.references)? != step.operation_reference_set_hash {
                return Err(Error::Protocol);
            }
            union.extend(step.references.iter().cloned());
        }
        if union.into_iter().collect::<Vec<_>>() != self.references {
            return Err(Error::Protocol);
        }
        use CredentialLedgerStepKindV1 as K;
        match (&self.inventory, &self.plan) {
            (CredentialLedgerInventoryV1::C24, CredentialLedgerPlanV1::Single { step })
                if step.kind == K::Set
                    && step.operation_reference_set_hash == self.operation_reference_set_hash => {}
            (CredentialLedgerInventoryV1::C25, CredentialLedgerPlanV1::Single { step })
                if matches!(step.kind, K::Delete | K::EnsureAbsent)
                    && step.operation_reference_set_hash == self.operation_reference_set_hash => {}
            (
                CredentialLedgerInventoryV1::C27 | CredentialLedgerInventoryV1::D37,
                CredentialLedgerPlanV1::Setup {
                    add,
                    prune,
                    registry_before,
                },
            ) => {
                registry_before.validate()?;
                if registry_before.kind != DocumentKindV2::Registry
                    || (add.is_none() && prune.is_none())
                    || add.as_ref().is_some_and(|s| {
                        s.kind != K::ApplyBatchAdd || !digest(&s.operation_reference_set_hash)
                    })
                    || prune.as_ref().is_some_and(|s| {
                        s.kind != K::PruneStale || !digest(&s.operation_reference_set_hash)
                    })
                {
                    return Err(Error::Protocol);
                }
            }
            _ => return Err(Error::Protocol),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerCredentialReceiptV1 {
    pub step_index: u8,
    pub step_kind: CredentialLedgerStepKindV1,
    pub operation_reference_set_hash: String,
    pub document: DocumentMetadataV2,
    pub managed_receipt: ManagedDocumentReceiptV1,
}
fn receipt_matches(
    receipt: &ManagedDocumentReceiptV1,
    after: &DocumentMetadataV2,
    operation: &CredentialLedgerDescriptorV1,
    before: &DocumentMetadataV2,
) -> Result<(), Error> {
    before.validate()?;
    after.validate()?;
    if receipt.operation_id != operation.operation_id
        || receipt.plan_hash != operation.plan_hash
        || receipt.document_kind != before.kind
        || after.kind != before.kind
        || receipt.before != *before
        || before.revision.checked_add(1) != Some(after.revision)
        || after.presence != DocumentPresenceV2::Present
        || receipt.after_revision != after.revision
        || receipt.after_token != after.token
        || before.token == after.token
        || !digest(&receipt.reference_set_hash)
        || (before.kind == DocumentKindV2::Registry) != receipt.registry_payload_hash.is_some()
        || receipt
            .registry_payload_hash
            .as_ref()
            .is_some_and(|v| !digest(v))
        || (DocumentReceiptV2::Managed {
            receipt: receipt.clone(),
        })
        .canonical_hash()?
            != after.receipt_hash
    {
        return Err(Error::Protocol);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerUnknownReasonV1 {
    ProcessRestart,
    EvidenceUnavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerProjectTerminalV1 {
    Completed,
    ReconciledAbandon,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerProjectCursorV1 {
    pub generation: u64,
    pub chain_hash: String,
    pub sequence: u64,
    pub journal_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerProjectEvidenceV1 {
    pub origin_project_identity: String,
    pub cursor: LedgerProjectCursorV1,
    pub state_hash: String,
}
impl LedgerProjectEvidenceV1 {
    fn validate(&self, origin: &str) -> Result<(), Error> {
        if self.origin_project_identity != origin
            || self.cursor.sequence == 0
            || !digest(&self.cursor.chain_hash)
            || !digest(&self.cursor.journal_hash)
            || !digest(&self.state_hash)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupersessionCertificateV1 {
    pub ledger_epoch_id: String,
    pub store_identity: String,
    pub origin_project_identity: String,
    pub root_operation_id: String,
    pub predecessor_operation_id: String,
    pub resolver_operation_id: String,
    pub direct_successor_operation_id: String,
    pub downstream_certificate_id: Option<String>,
    pub predecessor_depth: u8,
    pub resolver_depth: u8,
    pub predecessor_project: LedgerProjectEvidenceV1,
    pub predecessor_authorization_anchor_hash: String,
    pub resolver_project: LedgerProjectEvidenceV1,
    pub resolver_managed_receipt_id: String,
    pub resolver_after_token: String,
    pub resolver_after_revision: u64,
    pub resolver_credential_committed_transition_hash: String,
    pub previous_hash: String,
}
impl SupersessionCertificateV1 {
    pub fn canonical_hash(&self) -> Result<String, Error> {
        hash(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialLedgerEventV1 {
    Reserved {
        descriptor: CredentialLedgerDescriptorV1,
    },
    Armed {
        operation_id: String,
        project_authorization_anchor_hash: String,
    },
    OutcomeUnknown {
        operation_id: String,
        reason: LedgerUnknownReasonV1,
    },
    CredentialCommitted {
        operation_id: String,
        receipt: LedgerCredentialReceiptV1,
    },
    CredentialStepCommitted {
        operation_id: String,
        receipt: LedgerCredentialReceiptV1,
    },
    RegistryApplyStarted {
        operation_id: String,
    },
    RegistryCommitted {
        operation_id: String,
        document: DocumentMetadataV2,
        managed_receipt: ManagedDocumentReceiptV1,
    },
    RegistryNotApplied {
        operation_id: String,
        exact_before: DocumentMetadataV2,
    },
    RegistryOutcomeUnknown {
        operation_id: String,
    },
    Applied {
        operation_id: String,
    },
    CompensatedFailure {
        operation_id: String,
    },
    NoWriteCertified {
        operation_id: String,
        current: DocumentMetadataV2,
        operation_temp_absent: bool,
    },
    ProjectAcknowledged {
        operation_id: String,
        terminal: LedgerProjectTerminalV1,
        evidence: LedgerProjectEvidenceV1,
    },
    SupersessionCertified {
        certificate_id: String,
        certificate: SupersessionCertificateV1,
        current: DocumentMetadataV2,
    },
    CertificateAcknowledged {
        certificate_id: String,
        origin_project_identity: String,
        predecessor_project: LedgerProjectEvidenceV1,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialLedgerRecordV1 {
    pub schema_version: u32,
    pub ledger_epoch_id: String,
    pub sequence: u64,
    pub previous_hash: String,
    pub event: CredentialLedgerEventV1,
    pub record_hash: String,
}
#[derive(Serialize)]
struct RecordHashInput<'a> {
    schema_version: u32,
    ledger_epoch_id: &'a str,
    sequence: u64,
    previous_hash: &'a str,
    event: &'a CredentialLedgerEventV1,
}
impl CredentialLedgerRecordV1 {
    fn computed_hash(&self) -> Result<String, Error> {
        hash(&RecordHashInput {
            schema_version: self.schema_version,
            ledger_epoch_id: &self.ledger_epoch_id,
            sequence: self.sequence,
            previous_hash: &self.previous_hash,
            event: &self.event,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.record_hash != self.computed_hash()? {
            return Err(Error::Protocol);
        }
        bounded_json(self, MAX_LEDGER_RECORD_BYTES)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialLedgerPhaseV1 {
    Reserved,
    Armed,
    CredentialCommitted,
    RegistryApplyStarted,
    RegistryCommitted,
    RegistryNotApplied,
    RegistryOutcomeUnknown,
    Applied,
    CompensatedFailure,
    ReconciledAbandon,
    Superseded,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialLedgerOperationV1 {
    pub descriptor: CredentialLedgerDescriptorV1,
    pub reserved_entry_hash: String,
    pub project_authorization_anchor_hash: Option<String>,
    pub phase: CredentialLedgerPhaseV1,
    pub unknown: Option<LedgerUnknownReasonV1>,
    pub credential_receipts: Vec<LedgerCredentialReceiptV1>,
    pub credential_committed_transition_hash: Option<String>,
    pub registry_after: Option<DocumentMetadataV2>,
    pub acknowledgement: Option<LedgerProjectEvidenceV1>,
}
impl CredentialLedgerOperationV1 {
    fn unresolved(&self) -> bool {
        self.acknowledgement.is_none() && self.phase != CredentialLedgerPhaseV1::Superseded
    }
    fn credential_after(&self) -> &DocumentMetadataV2 {
        self.credential_receipts
            .last()
            .map_or(&self.descriptor.credential_before, |v| &v.document)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialLedgerCertificateV1 {
    pub certificate: SupersessionCertificateV1,
    pub acknowledgement: Option<LedgerProjectEvidenceV1>,
}
#[derive(Debug, Clone)]
pub struct CredentialLedgerSnapshotV1 {
    pub epoch: LedgerEpochV1,
    pub epoch_hash: String,
    pub sequence: u64,
    pub head_hash: String,
    pub operations: BTreeMap<String, CredentialLedgerOperationV1>,
    pub certificates: BTreeMap<String, CredentialLedgerCertificateV1>,
    encoded_bytes: usize,
}
impl CredentialLedgerSnapshotV1 {
    pub fn decode(epoch_bytes: &[u8], history_bytes: &[u8]) -> Result<Self, Error> {
        if epoch_bytes.is_empty()
            || epoch_bytes.len() > MAX_LEDGER_RECORD_BYTES
            || epoch_bytes
                .len()
                .checked_add(history_bytes.len())
                .is_none_or(|v| v > MAX_LEDGER_BYTES)
        {
            return Err(Error::Limit);
        }
        let epoch: LedgerEpochV1 =
            serde_json::from_slice(epoch_bytes).map_err(|_| Error::Protocol)?;
        if epoch.encode()? != epoch_bytes {
            return Err(Error::Protocol);
        }
        let epoch_hash = epoch.canonical_hash()?;
        let mut snapshot = Self {
            epoch,
            epoch_hash: epoch_hash.clone(),
            sequence: 0,
            head_hash: epoch_hash,
            operations: BTreeMap::new(),
            certificates: BTreeMap::new(),
            encoded_bytes: epoch_bytes.len(),
        };
        if !history_bytes.is_empty() && history_bytes.last() != Some(&b'\n') {
            return Err(Error::RecoveryRequired);
        }
        let complete = if history_bytes.is_empty() {
            history_bytes
        } else {
            &history_bytes[..history_bytes.len() - 1]
        };
        for line in complete
            .split(|b| *b == b'\n')
            .filter(|_| !history_bytes.is_empty())
        {
            if line.is_empty() || line.len() > MAX_LEDGER_RECORD_BYTES {
                return Err(Error::Limit);
            }
            let record: CredentialLedgerRecordV1 =
                serde_json::from_slice(line).map_err(|_| Error::Protocol)?;
            if record.encode()? != line {
                return Err(Error::Protocol);
            }
            snapshot.apply(&record, line.len() + 1)?;
        }
        Ok(snapshot)
    }
    /// Capacity must include the epoch and every unacknowledged or unknown record.
    pub fn reserve_capacity(&self, records: usize, operations: usize) -> Result<(), Error> {
        if records == 0
            || operations > records
            || (self.sequence as usize)
                .checked_add(1)
                .and_then(|v| v.checked_add(records))
                .is_none_or(|v| v > MAX_LEDGER_RECORDS)
            || self
                .operations
                .len()
                .checked_add(operations)
                .is_none_or(|v| v > MAX_LEDGER_OPERATIONS)
            || records
                .checked_mul(MAX_LEDGER_RECORD_BYTES + 1)
                .and_then(|v| self.encoded_bytes.checked_add(v))
                .is_none_or(|v| v > MAX_LEDGER_BYTES)
        {
            return Err(Error::Limit);
        }
        Ok(())
    }
    pub fn next_record(
        &self,
        event: CredentialLedgerEventV1,
    ) -> Result<CredentialLedgerRecordV1, Error> {
        let new_operation = usize::from(matches!(&event, CredentialLedgerEventV1::Reserved { .. }));
        self.reserve_capacity(1, new_operation)?;
        let mut record = CredentialLedgerRecordV1 {
            schema_version: 1,
            ledger_epoch_id: self.epoch.random_epoch_id.clone(),
            sequence: self.sequence.checked_add(1).ok_or(Error::Limit)?,
            previous_hash: self.head_hash.clone(),
            event,
            record_hash: String::new(),
        };
        record.record_hash = record.computed_hash()?;
        let encoded = record.encode()?;
        let mut candidate = self.clone();
        candidate.apply(&record, encoded.len() + 1)?;
        Ok(record)
    }
    pub fn operation_is_absent(&self, operation_id: &str) -> Result<bool, Error> {
        if !crate_operation_id(operation_id) {
            return Err(Error::Protocol);
        }
        Ok(!self.operations.contains_key(operation_id))
    }
    fn apply(&mut self, record: &CredentialLedgerRecordV1, encoded: usize) -> Result<(), Error> {
        if record.schema_version != 1
            || record.ledger_epoch_id != self.epoch.random_epoch_id
            || record.sequence != self.sequence.checked_add(1).ok_or(Error::Limit)?
            || record.previous_hash != self.head_hash
            || record.record_hash != record.computed_hash()?
            || !digest(&record.record_hash)
        {
            return Err(Error::Protocol);
        }
        if record.sequence >= MAX_LEDGER_RECORDS as u64
            || self
                .encoded_bytes
                .checked_add(encoded)
                .is_none_or(|v| v > MAX_LEDGER_BYTES)
        {
            return Err(Error::Limit);
        }
        self.apply_event(&record.event, &record.record_hash)?;
        self.sequence = record.sequence;
        self.head_hash = record.record_hash.clone();
        self.encoded_bytes += encoded;
        Ok(())
    }
    fn apply_event(
        &mut self,
        event: &CredentialLedgerEventV1,
        transition_hash: &str,
    ) -> Result<(), Error> {
        use CredentialLedgerEventV1 as E;
        use CredentialLedgerPhaseV1 as P;
        if let E::Reserved { descriptor } = event {
            if self
                .operations
                .values()
                .any(|o| matches!(o.phase, P::RegistryApplyStarted | P::RegistryOutcomeUnknown))
            {
                return Err(Error::RecoveryRequired);
            }
            descriptor.validate(&self.epoch)?;
            if self.operations.len() >= MAX_LEDGER_OPERATIONS {
                return Err(Error::Limit);
            }
            if self.operations.contains_key(&descriptor.operation_id)
                || self
                    .certificates
                    .values()
                    .any(|c| c.acknowledgement.is_none())
            {
                return Err(Error::RecoveryRequired);
            }
            match &descriptor.predecessor_operation_id {
                None if descriptor.chain_depth == 1
                    && descriptor.root_operation_id == descriptor.operation_id =>
                {
                    if self
                        .operations
                        .values()
                        .any(CredentialLedgerOperationV1::unresolved)
                    {
                        return Err(Error::RecoveryRequired);
                    }
                }
                Some(predecessor_id) => {
                    let predecessor = self.operations.get(predecessor_id).ok_or(Error::Protocol)?;
                    if !recoverable_predecessor(predecessor)
                        || predecessor.descriptor.chain_depth.checked_add(1)
                            != Some(descriptor.chain_depth)
                        || predecessor.descriptor.root_operation_id != descriptor.root_operation_id
                        || predecessor.descriptor.origin_project_identity
                            != descriptor.origin_project_identity
                        || predecessor.descriptor.operation_reference_set_hash
                            != descriptor.operation_reference_set_hash
                        || predecessor.descriptor.inventory != descriptor.inventory
                        || self.operations.values().any(|o| {
                            o.descriptor.predecessor_operation_id.as_ref() == Some(predecessor_id)
                        })
                        || self.operations.values().any(|o| {
                            o.unresolved()
                                && o.descriptor.root_operation_id != descriptor.root_operation_id
                        })
                    {
                        return Err(Error::RecoveryRequired);
                    }
                }
                _ => return Err(Error::Protocol),
            }
            self.operations.insert(
                descriptor.operation_id.clone(),
                CredentialLedgerOperationV1 {
                    descriptor: descriptor.clone(),
                    reserved_entry_hash: transition_hash.to_owned(),
                    project_authorization_anchor_hash: None,
                    phase: P::Reserved,
                    unknown: None,
                    credential_receipts: Vec::new(),
                    credential_committed_transition_hash: None,
                    registry_after: None,
                    acknowledgement: None,
                },
            );
            return Ok(());
        }
        if let E::SupersessionCertified {
            certificate_id,
            certificate,
            current,
        } = event
        {
            return self.certify(certificate_id, certificate, current);
        }
        if let E::CertificateAcknowledged {
            certificate_id,
            origin_project_identity,
            predecessor_project,
        } = event
        {
            let entry = self
                .certificates
                .get_mut(certificate_id)
                .ok_or(Error::Protocol)?;
            predecessor_project.validate(origin_project_identity)?;
            if entry.acknowledgement.is_some()
                || *origin_project_identity != entry.certificate.origin_project_identity
                || (
                    predecessor_project.cursor.generation,
                    predecessor_project.cursor.sequence,
                ) <= (
                    entry.certificate.predecessor_project.cursor.generation,
                    entry.certificate.predecessor_project.cursor.sequence,
                )
            {
                return Err(Error::Protocol);
            }
            entry.acknowledgement = Some(predecessor_project.clone());
            return Ok(());
        }
        let operation_id = match event {
            E::Armed { operation_id, .. }
            | E::OutcomeUnknown { operation_id, .. }
            | E::CredentialCommitted { operation_id, .. }
            | E::CredentialStepCommitted { operation_id, .. }
            | E::RegistryApplyStarted { operation_id }
            | E::RegistryCommitted { operation_id, .. }
            | E::RegistryNotApplied { operation_id, .. }
            | E::RegistryOutcomeUnknown { operation_id }
            | E::Applied { operation_id }
            | E::CompensatedFailure { operation_id }
            | E::NoWriteCertified { operation_id, .. }
            | E::ProjectAcknowledged { operation_id, .. } => operation_id,
            _ => return Err(Error::Protocol),
        };
        if self
            .operations
            .values()
            .any(|o| o.descriptor.predecessor_operation_id.as_ref() == Some(operation_id))
        {
            return Err(Error::RecoveryRequired);
        }
        let operation = self
            .operations
            .get_mut(operation_id)
            .ok_or(Error::Protocol)?;
        if operation.acknowledgement.is_some() || operation.phase == P::Superseded {
            return Err(Error::Protocol);
        }
        match event {
            E::Armed {
                project_authorization_anchor_hash,
                ..
            } => {
                if operation.phase != P::Reserved
                    || operation.unknown.is_some()
                    || operation.project_authorization_anchor_hash.is_some()
                    || !digest(project_authorization_anchor_hash)
                {
                    return Err(Error::Protocol);
                }
                operation.project_authorization_anchor_hash =
                    Some(project_authorization_anchor_hash.clone());
                operation.phase = P::Armed;
            }
            E::OutcomeUnknown { reason, .. } => {
                if operation.unknown.is_some()
                    || matches!(
                        operation.phase,
                        P::CredentialCommitted
                            | P::Applied
                            | P::CompensatedFailure
                            | P::ReconciledAbandon
                    )
                {
                    return Err(Error::Protocol);
                }
                operation.unknown = Some(*reason);
            }
            E::CredentialCommitted { receipt, .. } => {
                let CredentialLedgerPlanV1::Single { step } = &operation.descriptor.plan else {
                    return Err(Error::Protocol);
                };
                if operation.phase != P::Armed
                    || !operation.credential_receipts.is_empty()
                    || receipt.step_index != 0
                    || receipt.step_kind != step.kind
                    || receipt.operation_reference_set_hash != step.operation_reference_set_hash
                {
                    return Err(Error::Protocol);
                }
                validate_credential_receipt(operation, receipt)?;
                operation.credential_receipts.push(receipt.clone());
                operation.credential_committed_transition_hash = Some(transition_hash.to_owned());
                operation.phase = P::CredentialCommitted;
            }
            E::CredentialStepCommitted { receipt, .. } => {
                let CredentialLedgerPlanV1::Setup { add, prune, .. } = &operation.descriptor.plan
                else {
                    return Err(Error::Protocol);
                };
                use CredentialLedgerStepKindV1 as K;
                let planned = match (operation.phase, receipt.step_kind) {
                    (P::Armed, K::ApplyBatchAdd) if operation.credential_receipts.is_empty() => {
                        add.as_ref()
                    }
                    (P::RegistryCommitted, K::PruneStale)
                        if operation.credential_receipts.len() == usize::from(add.is_some()) =>
                    {
                        prune.as_ref()
                    }
                    (P::RegistryNotApplied, K::CompensateAdded)
                        if add.is_some() && operation.credential_receipts.len() == 1 =>
                    {
                        add.as_ref()
                    }
                    _ => None,
                }
                .ok_or(Error::Protocol)?;
                if receipt.step_index as usize != operation.credential_receipts.len()
                    || receipt.operation_reference_set_hash != planned.operation_reference_set_hash
                    || operation.credential_receipts.len() >= 2
                {
                    return Err(Error::Protocol);
                }
                validate_credential_receipt(operation, receipt)?;
                operation.credential_receipts.push(receipt.clone());
                operation.credential_committed_transition_hash = Some(transition_hash.to_owned());
            }
            E::RegistryApplyStarted { .. } => {
                let CredentialLedgerPlanV1::Setup { add, .. } = &operation.descriptor.plan else {
                    return Err(Error::Protocol);
                };
                if operation.phase != P::Armed
                    || operation.credential_receipts.len() != usize::from(add.is_some())
                {
                    return Err(Error::Protocol);
                }
                operation.phase = P::RegistryApplyStarted;
            }
            E::RegistryCommitted {
                document,
                managed_receipt,
                ..
            } => {
                let CredentialLedgerPlanV1::Setup {
                    registry_before, ..
                } = &operation.descriptor.plan
                else {
                    return Err(Error::Protocol);
                };
                if !matches!(
                    operation.phase,
                    P::RegistryApplyStarted | P::RegistryOutcomeUnknown
                ) {
                    return Err(Error::Protocol);
                }
                receipt_matches(
                    managed_receipt,
                    document,
                    &operation.descriptor,
                    registry_before,
                )?;
                operation.registry_after = Some(document.clone());
                operation.phase = P::RegistryCommitted;
            }
            E::RegistryNotApplied { exact_before, .. } => {
                let CredentialLedgerPlanV1::Setup {
                    registry_before, ..
                } = &operation.descriptor.plan
                else {
                    return Err(Error::Protocol);
                };
                if !matches!(
                    operation.phase,
                    P::RegistryApplyStarted | P::RegistryOutcomeUnknown
                ) || exact_before != registry_before
                {
                    return Err(Error::Protocol);
                }
                operation.phase = P::RegistryNotApplied;
            }
            E::RegistryOutcomeUnknown { .. } => {
                if operation.phase != P::RegistryApplyStarted {
                    return Err(Error::Protocol);
                }
                operation.phase = P::RegistryOutcomeUnknown;
            }
            E::Applied { .. } => {
                let CredentialLedgerPlanV1::Setup { add, prune, .. } = &operation.descriptor.plan
                else {
                    return Err(Error::Protocol);
                };
                if operation.phase != P::RegistryCommitted
                    || operation.credential_receipts.len()
                        != usize::from(add.is_some()) + usize::from(prune.is_some())
                {
                    return Err(Error::Protocol);
                }
                operation.phase = P::Applied;
            }
            E::CompensatedFailure { .. } => {
                let CredentialLedgerPlanV1::Setup { add, .. } = &operation.descriptor.plan else {
                    return Err(Error::Protocol);
                };
                if operation.phase != P::RegistryNotApplied
                    || operation.credential_receipts.len() != 2 * usize::from(add.is_some())
                {
                    return Err(Error::Protocol);
                }
                operation.phase = P::CompensatedFailure;
            }
            E::NoWriteCertified {
                current,
                operation_temp_absent,
                ..
            } => {
                if !matches!(operation.phase, P::Reserved | P::Armed)
                    || !operation_temp_absent
                    || current != &operation.descriptor.credential_before
                    || !operation.credential_receipts.is_empty()
                {
                    return Err(Error::Protocol);
                }
                operation.phase = P::ReconciledAbandon;
            }
            E::ProjectAcknowledged {
                terminal, evidence, ..
            } => {
                evidence.validate(&operation.descriptor.origin_project_identity)?;
                let expected = match operation.phase {
                    P::CredentialCommitted | P::Applied | P::CompensatedFailure => {
                        LedgerProjectTerminalV1::Completed
                    }
                    P::ReconciledAbandon => LedgerProjectTerminalV1::ReconciledAbandon,
                    _ => return Err(Error::Protocol),
                };
                if *terminal != expected {
                    return Err(Error::Protocol);
                }
                operation.acknowledgement = Some(evidence.clone());
            }
            _ => return Err(Error::Protocol),
        }
        Ok(())
    }
    fn certify(
        &mut self,
        id: &str,
        certificate: &SupersessionCertificateV1,
        current: &DocumentMetadataV2,
    ) -> Result<(), Error> {
        use CredentialLedgerPhaseV1 as P;
        let predecessor = self
            .operations
            .get(&certificate.predecessor_operation_id)
            .ok_or(Error::Protocol)?;
        let resolver = self
            .operations
            .get(&certificate.resolver_operation_id)
            .ok_or(Error::Protocol)?;
        let successor = self
            .operations
            .get(&certificate.direct_successor_operation_id)
            .ok_or(Error::Protocol)?;
        certificate
            .predecessor_project
            .validate(&certificate.origin_project_identity)?;
        certificate
            .resolver_project
            .validate(&certificate.origin_project_identity)?;
        if certificate.canonical_hash()? != id
            || self.certificates.contains_key(id)
            || certificate.previous_hash != self.head_hash
            || certificate.ledger_epoch_id != self.epoch.random_epoch_id
            || certificate.store_identity != self.epoch.store_identity
            || !recoverable_predecessor(predecessor)
            || !matches!(resolver.phase, P::CredentialCommitted | P::Applied)
            || predecessor.descriptor.origin_project_identity != certificate.origin_project_identity
            || resolver.descriptor.origin_project_identity != certificate.origin_project_identity
            || predecessor.descriptor.root_operation_id != certificate.root_operation_id
            || resolver.descriptor.root_operation_id != certificate.root_operation_id
            || predecessor.descriptor.operation_reference_set_hash
                != resolver.descriptor.operation_reference_set_hash
            || predecessor.descriptor.inventory != resolver.descriptor.inventory
            || successor.descriptor.predecessor_operation_id.as_ref()
                != Some(&certificate.predecessor_operation_id)
            || predecessor.descriptor.chain_depth != certificate.predecessor_depth
            || resolver.descriptor.chain_depth != certificate.resolver_depth
            || certificate.predecessor_depth >= certificate.resolver_depth
            || predecessor
                .project_authorization_anchor_hash
                .as_ref()
                .unwrap_or(&predecessor.descriptor.authorization_anchor_hash)
                != &certificate.predecessor_authorization_anchor_hash
            || resolver.credential_committed_transition_hash.as_ref()
                != Some(&certificate.resolver_credential_committed_transition_hash)
            || resolver.credential_after() != current
            || current.receipt_hash != certificate.resolver_managed_receipt_id
            || current.token != certificate.resolver_after_token
            || current.revision != certificate.resolver_after_revision
        {
            return Err(Error::Protocol);
        }
        if successor.descriptor.operation_id == resolver.descriptor.operation_id {
            if certificate.downstream_certificate_id.is_some() {
                return Err(Error::Protocol);
            }
        } else {
            let downstream = certificate
                .downstream_certificate_id
                .as_ref()
                .and_then(|v| self.certificates.get(v))
                .ok_or(Error::Protocol)?;
            if downstream.certificate.predecessor_operation_id != successor.descriptor.operation_id
                || downstream.certificate.resolver_operation_id != resolver.descriptor.operation_id
            {
                return Err(Error::Protocol);
            }
        }
        self.operations
            .get_mut(&certificate.predecessor_operation_id)
            .ok_or(Error::Protocol)?
            .phase = P::Superseded;
        self.certificates.insert(
            id.to_owned(),
            CredentialLedgerCertificateV1 {
                certificate: certificate.clone(),
                acknowledgement: None,
            },
        );
        Ok(())
    }
}
fn validate_credential_receipt(
    operation: &CredentialLedgerOperationV1,
    receipt: &LedgerCredentialReceiptV1,
) -> Result<(), Error> {
    if receipt.document.kind != DocumentKindV2::Credential
        || !digest(&receipt.operation_reference_set_hash)
    {
        return Err(Error::Protocol);
    }
    receipt_matches(
        &receipt.managed_receipt,
        &receipt.document,
        &operation.descriptor,
        operation.credential_after(),
    )
}

#[cfg(test)]
mod tests;

fn recoverable_predecessor(operation: &CredentialLedgerOperationV1) -> bool {
    operation.unknown.is_some()
        && operation.acknowledgement.is_none()
        && matches!(
            operation.phase,
            CredentialLedgerPhaseV1::Reserved | CredentialLedgerPhaseV1::Armed
        )
}
