//! C10's closed, immutable identity and complete durable history. No transport.

use super::{
    ExpectedSyncRemoteOidV1, SyncDigestV1, SyncGitObjectIdV1, SyncRemoteReceiptIdV1,
    SyncRemoteRefV1, SyncShareIdV1,
};
use crate::management_effect::ManagementOperationIdV1;
use polaris_auth::git_credentials::{CanonicalHttpsRemoteV1, GitCredentialBindingV1};
use serde::{Deserialize, Serialize};

pub const MAX_SYNC_INTENT_BYTES_V2: usize = 128 * 1024;
pub const MAX_SYNC_INTENT_GENERATIONS_V2: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("c10_invalid_intent: {0}")]
pub struct SyncIntentValueErrorV2(pub &'static str);
type Result<T> = std::result::Result<T, SyncIntentValueErrorV2>;

fn invalid(what: &'static str) -> SyncIntentValueErrorV2 {
    SyncIntentValueErrorV2(what)
}

fn hash<T: Serialize>(domain: &[u8], value: &T) -> Result<SyncDigestV1> {
    let body = serde_json::to_vec(value).map_err(|_| invalid("encoding"))?;
    let mut input = Vec::with_capacity(domain.len() + body.len());
    input.extend_from_slice(domain);
    input.extend_from_slice(&body);
    Ok(SyncDigestV1::digest(&input))
}

pub(crate) fn zero_digest_v2() -> SyncDigestV1 {
    SyncDigestV1::from_hex(&"0".repeat(64)).expect("fixed zero digest")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPayloadEntryV1 {
    pub path: String,
    pub mode: SyncPayloadModeV1,
    pub bytes: u64,
    pub content_hash: SyncDigestV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SyncPayloadModeV1 {
    #[serde(rename = "100644")]
    Regular,
    #[serde(rename = "100755")]
    Executable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPayloadDescriptorV1 {
    entry_count: usize,
    total_bytes: u64,
    digest: SyncDigestV1,
}

impl SyncPayloadDescriptorV1 {
    /// Input must already have the exact Git tree order; never normalize it.
    pub fn new(entries: Vec<SyncPayloadEntryV1>) -> Result<Self> {
        use crate::sync::repository::{MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES};
        if entries.len() > MAX_FILES {
            return Err(invalid("payload count"));
        }
        let mut previous: Option<&str> = None;
        let mut total_bytes = 0_u64;
        for entry in &entries {
            if entry.path.is_empty()
                || entry.path.starts_with('/')
                || entry.path.contains('\\')
                || entry.path.bytes().any(|byte| byte.is_ascii_control())
                || entry.path.split('/').any(|part| {
                    part.is_empty()
                        || part == "."
                        || part == ".."
                        || part.eq_ignore_ascii_case(".git")
                })
                || previous.is_some_and(|path| path >= entry.path.as_str())
            {
                return Err(invalid("payload path"));
            }
            if entry.bytes > MAX_FILE_BYTES {
                return Err(invalid("payload file size"));
            }
            previous = Some(&entry.path);
            total_bytes = total_bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| invalid("payload size"))?;
            if total_bytes > MAX_TOTAL_BYTES {
                return Err(invalid("payload size"));
            }
        }
        let entry_count = entries.len();
        let digest = hash(
            b"polaris-sync-payload-commitment-v1\0",
            &(entry_count, total_bytes, &entries),
        )?;
        let value = Self {
            entry_count,
            total_bytes,
            digest,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn entry_count(&self) -> usize {
        self.entry_count
    }
    pub fn digest(&self) -> SyncDigestV1 {
        self.digest
    }
    #[cfg(test)]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    pub fn validate(&self) -> Result<()> {
        use crate::sync::repository::{MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES};
        if self.entry_count > MAX_FILES
            || self.total_bytes > MAX_TOTAL_BYTES
            || (self.entry_count as u64)
                .checked_mul(MAX_FILE_BYTES)
                .is_none_or(|maximum| self.total_bytes > maximum)
            || self.entry_count == 0
                && self.digest
                    != hash(
                        b"polaris-sync-payload-commitment-v1\0",
                        &(0_usize, 0_u64, Vec::<SyncPayloadEntryV1>::new()),
                    )?
        {
            return Err(invalid("payload commitment"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncIntentIdentityV2 {
    pub operation_id: ManagementOperationIdV1,
    pub request_hash: SyncDigestV1,
    pub project: SyncDigestV1,
    pub share_id: SyncShareIdV1,
    pub remote: CanonicalHttpsRemoteV1,
    pub remote_ref: SyncRemoteRefV1,
    pub idempotency_hash: SyncDigestV1,
    pub account_binding: GitCredentialBindingV1,
    pub permission_hash: SyncDigestV1,
    pub payload: SyncPayloadDescriptorV1,
    pub prior_participation_digest: SyncDigestV1,
    pub expected_remote_oid: ExpectedSyncRemoteOidV1,
}

impl SyncIntentIdentityV2 {
    pub fn request_hash_for_prepared(
        &self,
        prepared: &SyncLocalPreparedV2,
    ) -> Result<SyncDigestV1> {
        prepared.validate(self)?;
        hash(
            b"polaris-c10-management-request-v2\0",
            &(
                self.operation_id,
                self.project,
                &self.share_id,
                &self.remote,
                &self.remote_ref,
                &self.account_binding,
                self.permission_hash,
                &self.payload,
                self.prior_participation_digest,
                &self.expected_remote_oid,
                prepared,
            ),
        )
    }

    /// Finalize one immutable request from the complete read-only P/H/A/R snapshot.
    pub fn bind_local_request(mut self, prepared: &SyncLocalPreparedV2) -> Result<Self> {
        self.request_hash = self.request_hash_for_prepared(prepared)?;
        self.idempotency_hash = Self::idempotency_hash(self.operation_id, self.request_hash);
        self.validate()?;
        Ok(self)
    }
    /// The management domain is shared with other effects. The request hash
    /// additionally binds the complete immutable credential snapshot.
    pub fn management_account_binding_hash(
        &self,
    ) -> Result<crate::management_effect::ManagementEffectHashV1> {
        crate::management_effect::ManagementEffectHashV1::account_binding(
            "git-keychain",
            &format!(
                "{}:{}",
                self.account_binding.credential_item_id, self.account_binding.generation
            ),
        )
        .map_err(|_| invalid("management account binding"))
    }
    /// The key binds the exact operation and already finalized management request.
    pub fn idempotency_hash(
        operation: ManagementOperationIdV1,
        request: SyncDigestV1,
    ) -> SyncDigestV1 {
        hash(b"polaris-c10-idempotency-v2\0", &(operation, request))
            .expect("fixed identity encoding")
    }

    pub fn validate(&self) -> Result<()> {
        self.account_binding
            .validate()
            .map_err(|_| invalid("credential binding"))?;
        self.payload.validate()?;
        if self.account_binding.remote != self.remote
            || self.remote_ref.as_str() != "refs/heads/polaris"
            || self.idempotency_hash != Self::idempotency_hash(self.operation_id, self.request_hash)
            || self.request_hash == zero_digest_v2()
            || self.permission_hash == zero_digest_v2()
        {
            return Err(invalid("identity"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "head", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncPriorHeadV2 {
    Unborn,
    Present { commit: SyncGitObjectIdV1 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncLocalPreparedV2 {
    pub payload: SyncPayloadDescriptorV1,
    pub prior_head: SyncPriorHeadV2,
    pub head_tree: SyncPayloadDescriptorV1,
    pub agreed_commit: Option<SyncGitObjectIdV1>,
    pub expected_remote: ExpectedSyncRemoteOidV1,
    pub prior_index_digest: SyncDigestV1,
    pub prior_staging_digest: SyncDigestV1,
}

impl SyncLocalPreparedV2 {
    fn validate(&self, identity: &SyncIntentIdentityV2) -> Result<()> {
        self.payload.validate()?;
        self.head_tree.validate()?;
        if self.payload != identity.payload
            || self.expected_remote != identity.expected_remote_oid
            || matches!(self.prior_head, SyncPriorHeadV2::Unborn) && !self.head_tree.is_empty()
        {
            return Err(invalid("local prepared"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncBeforeSendV2 {
    pub intended_commit: SyncGitObjectIdV1,
    pub reopened_descriptor: SyncPayloadDescriptorV1,
    pub expected_remote: ExpectedSyncRemoteOidV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "observation", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRemoteObservationV2 {
    Unavailable,
    Missing,
    Present { oid: SyncGitObjectIdV1 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPreviousStateV2 {
    LocalPrepared,
    BeforeSend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncUnknownReasonV2 {
    LocalValidationFailed,
    RestartedAfterLocalPrepared,
    RestartedAfterBeforeSend,
    CredentialBindingChanged,
    TransportUncertain,
    RemoteRefDidNotMatch,
    ObservationUnavailable,
    ReceiptPersistenceFailed,
    AbandonRequested,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncOutcomeUnknownV2 {
    pub previous_state: SyncPreviousStateV2,
    pub intended_commit: Option<SyncGitObjectIdV1>,
    pub created_commit_proof: Option<SyncCreatedCommitProofV2>,
    pub last_observation: SyncRemoteObservationV2,
    pub reason: SyncUnknownReasonV2,
}

/// Persisted projection of P3's actual durability and Git reopen readback.
/// This projection is structural evidence; its hash alone is not fsync proof.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncCreatedCommitProofV2 {
    operation_id: ManagementOperationIdV1,
    request_hash: SyncDigestV1,
    commit: SyncGitObjectIdV1,
    prior_head: SyncPriorHeadV2,
    reopened_descriptor: SyncPayloadDescriptorV1,
    reopened_objects_digest: SyncDigestV1,
    evidence_hash: SyncDigestV1,
}

impl SyncCreatedCommitProofV2 {
    pub(crate) fn commit(&self) -> &SyncGitObjectIdV1 {
        &self.commit
    }

    /// Compare a validated persisted projection with a new actual Git
    /// durability/reopen readback. Fetch may legitimately change the object
    /// inventory: each digest proves its own fsync/reopen cohort, not a
    /// permanent repository-wide equality.
    pub(crate) fn require_actual_readback(
        &self,
        actual: &crate::sync::repository::c10::C10CommitReadbackV1,
    ) -> Result<()> {
        self.reopened_descriptor.validate()?;
        if self.evidence_hash != self.computed_hash()?
            || !actual.created()
            || actual.operation_id() != self.operation_id
            || actual.request_hash() != self.request_hash
            || actual.commit() != &self.commit
            || actual.prior_head() != &self.prior_head
            || actual.descriptor() != &self.reopened_descriptor
            || actual.objects_digest() == zero_digest_v2()
            || self.reopened_objects_digest == zero_digest_v2()
        {
            return Err(invalid("actual created commit readback"));
        }
        Ok(())
    }

    fn computed_hash(&self) -> Result<SyncDigestV1> {
        hash(
            b"polaris-c10-created-git-reopen-v2\0",
            &(
                self.operation_id,
                self.request_hash,
                &self.commit,
                &self.prior_head,
                &self.reopened_descriptor,
                self.reopened_objects_digest,
            ),
        )
    }
    fn validate(
        &self,
        identity: &SyncIntentIdentityV2,
        prior_head: &SyncPriorHeadV2,
    ) -> Result<()> {
        self.reopened_descriptor.validate()?;
        if self.operation_id != identity.operation_id
            || self.request_hash != identity.request_hash
            || self.prior_head != *prior_head
            || self.reopened_descriptor != identity.payload
            || self.reopened_objects_digest == zero_digest_v2()
            || self.evidence_hash != self.computed_hash()?
        {
            return Err(invalid("created commit reopen proof"));
        }
        Ok(())
    }
}

/// Only the actual P3 durability/reopen owner may mint this capability.
/// P2b exposes no production mint from a caller hash, boolean or persisted DTO.
pub(crate) struct C10CreatedCommitReopenProofV2 {
    persisted: SyncCreatedCommitProofV2,
}

impl C10CreatedCommitReopenProofV2 {
    /// Consume the repository owner's actual fsync/close/reopen readback.
    /// A persisted DTO, caller digest, or merely re-used HEAD cannot mint it.
    pub(crate) fn from_git_readback(
        readback: crate::sync::repository::c10::C10CommitReadbackV1,
    ) -> Result<Self> {
        if !readback.created() {
            return Err(invalid("readback did not create a commit"));
        }
        let mut persisted = SyncCreatedCommitProofV2 {
            operation_id: readback.operation_id(),
            request_hash: readback.request_hash(),
            commit: readback.commit().clone(),
            prior_head: readback.prior_head().clone(),
            reopened_descriptor: readback.descriptor().clone(),
            reopened_objects_digest: readback.objects_digest(),
            evidence_hash: zero_digest_v2(),
        };
        if persisted.reopened_objects_digest == zero_digest_v2() {
            return Err(invalid("empty Git object readback evidence"));
        }
        persisted.evidence_hash = persisted.computed_hash()?;
        Ok(Self { persisted })
    }
}

#[cfg(test)]
pub(crate) fn c10_test_created_reopen_cap(
    identity: &SyncIntentIdentityV2,
    prior_head: &SyncPriorHeadV2,
    commit: SyncGitObjectIdV1,
) -> C10CreatedCommitReopenProofV2 {
    let mut persisted = SyncCreatedCommitProofV2 {
        operation_id: identity.operation_id,
        request_hash: identity.request_hash,
        commit,
        prior_head: prior_head.clone(),
        reopened_descriptor: identity.payload.clone(),
        reopened_objects_digest: SyncDigestV1::digest(b"synthetic actual Git reopen fixture"),
        evidence_hash: zero_digest_v2(),
    };
    persisted.evidence_hash = persisted.computed_hash().unwrap();
    C10CreatedCommitReopenProofV2 { persisted }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncReceiptSourceV2 {
    TransportAndExactRef { receipt_id: SyncRemoteReceiptIdV1 },
    ReconciledExactRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncCompletedReceiptV2 {
    pub identity: SyncIntentIdentityV2,
    pub intended_commit: SyncGitObjectIdV1,
    pub expected_remote: ExpectedSyncRemoteOidV1,
    pub observed_oid: SyncGitObjectIdV1,
    pub payload: SyncPayloadDescriptorV1,
    pub participation_digest: SyncDigestV1,
    pub source: SyncReceiptSourceV2,
}

impl SyncCompletedReceiptV2 {
    fn validate(&self, identity: &SyncIntentIdentityV2) -> Result<()> {
        self.identity.validate()?;
        self.payload.validate()?;
        if self.identity != *identity
            || self.payload != identity.payload
            || self.expected_remote != identity.expected_remote_oid
            || self.intended_commit != self.observed_oid
            || self.participation_digest == zero_digest_v2()
        {
            return Err(invalid("completed receipt"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAbandonConfirmationV2 {
    ExplicitUserConfirmation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoIntentAbandonEvidenceV2 {
    pub identity: SyncIntentIdentityV2,
    pub prior_head: SyncPriorHeadV2,
    pub current_head: SyncPriorHeadV2,
    pub agreed_commit: Option<SyncGitObjectIdV1>,
    pub prior_descriptor: SyncPayloadDescriptorV1,
    pub current_descriptor: SyncPayloadDescriptorV1,
    pub prior_index_digest: SyncDigestV1,
    pub current_index_digest: SyncDigestV1,
    pub prior_staging_digest: SyncDigestV1,
    pub current_staging_digest: SyncDigestV1,
    pub last_observation: SyncRemoteObservationV2,
    pub active_inventory_digest: SyncDigestV1,
    pub confirmation: SyncAbandonConfirmationV2,
}

impl NoIntentAbandonEvidenceV2 {
    fn validate(&self, identity: &SyncIntentIdentityV2) -> Result<()> {
        self.identity.validate()?;
        self.prior_descriptor.validate()?;
        self.current_descriptor.validate()?;
        let prepared = SyncLocalPreparedV2 {
            payload: identity.payload.clone(),
            prior_head: self.prior_head.clone(),
            head_tree: self.prior_descriptor.clone(),
            agreed_commit: self.agreed_commit.clone(),
            expected_remote: identity.expected_remote_oid.clone(),
            prior_index_digest: self.prior_index_digest,
            prior_staging_digest: self.prior_staging_digest,
        };
        if self.identity != *identity
            || self.prior_head != self.current_head
            || identity.request_hash != identity.request_hash_for_prepared(&prepared)?
            || self.prior_descriptor != self.current_descriptor
            || self.prior_index_digest != self.current_index_digest
            || self.prior_staging_digest != self.current_staging_digest
            || matches!(self.prior_head, SyncPriorHeadV2::Unborn)
                && !self.prior_descriptor.is_empty()
            || self.active_inventory_digest == zero_digest_v2()
        {
            return Err(invalid("no intent abandon evidence"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredLocalAbandonEvidenceV2 {
    pub identity: SyncIntentIdentityV2,
    pub prior_head: SyncPriorHeadV2,
    pub expected_current_head: SyncPriorHeadV2,
    pub created_commit: Option<SyncGitObjectIdV1>,
    pub restored_head: SyncPriorHeadV2,
    pub prior_descriptor: SyncPayloadDescriptorV1,
    pub reopened_descriptor: SyncPayloadDescriptorV1,
    pub prior_index_digest: SyncDigestV1,
    pub restored_index_digest: SyncDigestV1,
    pub prior_staging_digest: SyncDigestV1,
    pub restored_staging_digest: SyncDigestV1,
    pub durable_restore_digest: SyncDigestV1,
    pub last_observation: SyncRemoteObservationV2,
    pub participation_digest: SyncDigestV1,
    pub confirmation: SyncAbandonConfirmationV2,
}

impl RestoredLocalAbandonEvidenceV2 {
    fn validate(&self, identity: &SyncIntentIdentityV2) -> Result<()> {
        self.identity.validate()?;
        self.prior_descriptor.validate()?;
        self.reopened_descriptor.validate()?;
        if self.identity != *identity
            || self.prior_head != self.restored_head
            || self.prior_descriptor != self.reopened_descriptor
            || self.prior_index_digest != self.restored_index_digest
            || self.prior_staging_digest != self.restored_staging_digest
            || self.participation_digest != identity.prior_participation_digest
            || self.durable_restore_digest == zero_digest_v2()
            || matches!(self.prior_head, SyncPriorHeadV2::Unborn)
                && !self.prior_descriptor.is_empty()
            || self.created_commit.as_ref().is_some_and(|created| {
                self.expected_current_head
                    != (SyncPriorHeadV2::Present {
                        commit: created.clone(),
                    })
            })
            || self.created_commit.is_none() && self.expected_current_head != self.prior_head
        {
            return Err(invalid("restored local abandon evidence"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncAbandonEvidenceV2 {
    NoIntent {
        evidence: Box<NoIntentAbandonEvidenceV2>,
    },
    RestoredLocal {
        evidence: Box<RestoredLocalAbandonEvidenceV2>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRemoteIntentStateV2 {
    LocalPrepared {
        prepared: Box<SyncLocalPreparedV2>,
    },
    BeforeSend {
        before_send: Box<SyncBeforeSendV2>,
    },
    OutcomeUnknown {
        unknown: Box<SyncOutcomeUnknownV2>,
    },
    Completed {
        receipt: Box<SyncCompletedReceiptV2>,
    },
    ReconciledAbandon {
        evidence: Box<SyncAbandonEvidenceV2>,
    },
}

impl SyncRemoteIntentStateV2 {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::ReconciledAbandon { .. }
        )
    }

    fn validate(&self, identity: &SyncIntentIdentityV2) -> Result<()> {
        match self {
            Self::LocalPrepared { prepared } => prepared.validate(identity),
            Self::BeforeSend { before_send } => {
                before_send.reopened_descriptor.validate()?;
                if before_send.reopened_descriptor != identity.payload
                    || before_send.expected_remote != identity.expected_remote_oid
                {
                    return Err(invalid("before send"));
                }
                Ok(())
            }
            Self::OutcomeUnknown { unknown } => {
                if unknown.previous_state == SyncPreviousStateV2::BeforeSend
                    && unknown.intended_commit.is_none()
                {
                    return Err(invalid("unknown intended commit"));
                }
                if let Some(proof) = &unknown.created_commit_proof {
                    proof.validate(identity, &proof.prior_head)?;
                }
                Ok(())
            }
            Self::Completed { receipt } => receipt.validate(identity),
            Self::ReconciledAbandon { evidence } => match evidence.as_ref() {
                SyncAbandonEvidenceV2::NoIntent { evidence } => evidence.validate(identity),
                SyncAbandonEvidenceV2::RestoredLocal { evidence } => evidence.validate(identity),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoteIntentRecordV2 {
    schema_version: u8,
    identity: SyncIntentIdentityV2,
    sequence: u64,
    previous_record_hash: SyncDigestV1,
    state: SyncRemoteIntentStateV2,
    record_hash: SyncDigestV1,
}

#[derive(Serialize)]
struct HashInput<'a> {
    schema_version: u8,
    identity: &'a SyncIntentIdentityV2,
    sequence: u64,
    previous_record_hash: SyncDigestV1,
    state: &'a SyncRemoteIntentStateV2,
}

impl SyncRemoteIntentRecordV2 {
    pub fn identity(&self) -> &SyncIntentIdentityV2 {
        &self.identity
    }
    pub fn state(&self) -> &SyncRemoteIntentStateV2 {
        &self.state
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn record_hash(&self) -> SyncDigestV1 {
        self.record_hash
    }
    pub fn previous_record_hash(&self) -> SyncDigestV1 {
        self.previous_record_hash
    }

    fn computed_hash(&self) -> Result<SyncDigestV1> {
        hash(
            b"polaris-c10-intent-record-v2\0",
            &HashInput {
                schema_version: self.schema_version,
                identity: &self.identity,
                sequence: self.sequence,
                previous_record_hash: self.previous_record_hash,
                state: &self.state,
            },
        )
    }

    fn validate(&self) -> Result<()> {
        self.identity.validate()?;
        self.state.validate(&self.identity)?;
        if self.schema_version != 2
            || self.sequence == 0
            || self.record_hash != self.computed_hash()?
        {
            return Err(invalid("record hash"));
        }
        Ok(())
    }

    fn new(
        identity: SyncIntentIdentityV2,
        sequence: u64,
        previous_record_hash: SyncDigestV1,
        state: SyncRemoteIntentStateV2,
    ) -> Result<Self> {
        let mut value = Self {
            schema_version: 2,
            identity,
            sequence,
            previous_record_hash,
            state,
            record_hash: zero_digest_v2(),
        };
        value.record_hash = value.computed_hash()?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoteIntentHistoryV2 {
    schema_version: u8,
    records: Vec<SyncRemoteIntentRecordV2>,
}

impl SyncRemoteIntentHistoryV2 {
    pub fn local_prepared(
        identity: SyncIntentIdentityV2,
        prepared: SyncLocalPreparedV2,
    ) -> Result<Self> {
        if identity.request_hash != identity.request_hash_for_prepared(&prepared)? {
            return Err(invalid("request hash"));
        }
        Self::first(
            identity,
            SyncRemoteIntentStateV2::LocalPrepared {
                prepared: Box::new(prepared),
            },
        )
    }

    pub fn no_intent_abandon(evidence: NoIntentAbandonEvidenceV2) -> Result<Self> {
        Self::first(
            evidence.identity.clone(),
            SyncRemoteIntentStateV2::ReconciledAbandon {
                evidence: Box::new(SyncAbandonEvidenceV2::NoIntent {
                    evidence: Box::new(evidence),
                }),
            },
        )
    }

    fn first(identity: SyncIntentIdentityV2, state: SyncRemoteIntentStateV2) -> Result<Self> {
        let value = Self {
            schema_version: 2,
            records: vec![SyncRemoteIntentRecordV2::new(
                identity,
                1,
                zero_digest_v2(),
                state,
            )?],
        };
        value.validate()?;
        value.canonical_bytes()?;
        Ok(value)
    }

    pub fn current(&self) -> &SyncRemoteIntentRecordV2 {
        self.records.last().expect("validated nonempty history")
    }
    pub fn records(&self) -> &[SyncRemoteIntentRecordV2] {
        &self.records
    }
    pub fn operation_id(&self) -> ManagementOperationIdV1 {
        self.current().identity.operation_id
    }
    pub fn is_terminal(&self) -> bool {
        self.current().state.is_terminal()
    }

    pub fn before_send(&self, before_send: SyncBeforeSendV2) -> Result<Self> {
        self.append(SyncRemoteIntentStateV2::BeforeSend {
            before_send: Box::new(before_send),
        })
    }
    pub fn outcome_unknown(&self, unknown: SyncOutcomeUnknownV2) -> Result<Self> {
        if unknown.created_commit_proof.is_some() {
            return Err(invalid("created reopen capability required"));
        }
        self.append(SyncRemoteIntentStateV2::OutcomeUnknown {
            unknown: Box::new(unknown),
        })
    }
    pub(crate) fn outcome_unknown_after_reopen(
        &self,
        mut unknown: SyncOutcomeUnknownV2,
        proof: C10CreatedCommitReopenProofV2,
    ) -> Result<Self> {
        let SyncRemoteIntentStateV2::LocalPrepared { prepared } = self.current().state() else {
            return Err(invalid("created reopen origin"));
        };
        if unknown.created_commit_proof.is_some() {
            return Err(invalid("created reopen capability required"));
        }
        proof
            .persisted
            .validate(self.current().identity(), &prepared.prior_head)?;
        unknown.created_commit_proof = Some(proof.persisted);
        self.append(SyncRemoteIntentStateV2::OutcomeUnknown {
            unknown: Box::new(unknown),
        })
    }
    pub fn completed(&self, receipt: SyncCompletedReceiptV2) -> Result<Self> {
        self.append(SyncRemoteIntentStateV2::Completed {
            receipt: Box::new(receipt),
        })
    }
    pub fn reconciled_abandon(&self, evidence: RestoredLocalAbandonEvidenceV2) -> Result<Self> {
        self.append(SyncRemoteIntentStateV2::ReconciledAbandon {
            evidence: Box::new(SyncAbandonEvidenceV2::RestoredLocal {
                evidence: Box::new(evidence),
            }),
        })
    }

    fn append(&self, state: SyncRemoteIntentStateV2) -> Result<Self> {
        self.validate()?;
        let current = self.current();
        let sequence = current
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("sequence overflow"))?;
        let next = SyncRemoteIntentRecordV2::new(
            current.identity.clone(),
            sequence,
            current.record_hash,
            state,
        )?;
        validate_transition(current, &next)?;
        let mut value = self.clone();
        value.records.push(next);
        value.validate()?;
        value.canonical_bytes()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 2
            || self.records.is_empty()
            || self.records.len() > MAX_SYNC_INTENT_GENERATIONS_V2
        {
            return Err(invalid("history generations"));
        }
        let first = &self.records[0];
        let valid_first_state = match &first.state {
            SyncRemoteIntentStateV2::LocalPrepared { .. } => true,
            SyncRemoteIntentStateV2::ReconciledAbandon { evidence } => {
                matches!(evidence.as_ref(), SyncAbandonEvidenceV2::NoIntent { .. })
            }
            _ => false,
        };
        if first.sequence != 1
            || first.previous_record_hash != zero_digest_v2()
            || !valid_first_state
        {
            return Err(invalid("first generation"));
        }
        for record in &self.records {
            record.validate()?;
        }
        if let SyncRemoteIntentStateV2::LocalPrepared { prepared } = first.state()
            && first.identity.request_hash != first.identity.request_hash_for_prepared(prepared)?
        {
            return Err(invalid("request hash"));
        }
        for pair in self.records.windows(2) {
            validate_transition(&pair[0], &pair[1])?;
        }
        if let SyncRemoteIntentStateV2::ReconciledAbandon { evidence } = self.current().state()
            && let SyncAbandonEvidenceV2::RestoredLocal { evidence } = evidence.as_ref()
        {
            let SyncRemoteIntentStateV2::LocalPrepared { prepared } = first.state() else {
                return Err(invalid("restore origin"));
            };
            if evidence.prior_head != prepared.prior_head
                || evidence.prior_descriptor != prepared.head_tree
                || evidence.prior_index_digest != prepared.prior_index_digest
                || evidence.prior_staging_digest != prepared.prior_staging_digest
            {
                return Err(invalid("restore prior identity"));
            }
        }
        Ok(())
    }

    pub fn terminal_evidence_hash(&self) -> Result<SyncDigestV1> {
        self.validate()?;
        if !self.is_terminal() {
            return Err(invalid("nonterminal evidence"));
        }
        hash(b"polaris-c10-terminal-evidence-v2\0", self.current())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let body = serde_json::to_vec(self).map_err(|_| invalid("encoding"))?;
        if body.len() > MAX_SYNC_INTENT_BYTES_V2 {
            return Err(invalid("record size"));
        }
        Ok(body)
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_SYNC_INTENT_BYTES_V2 {
            return Err(invalid("record size"));
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| invalid("closed schema"))?;
        if value.canonical_bytes()? != bytes {
            return Err(invalid("noncanonical record"));
        }
        Ok(value)
    }

    pub(crate) fn is_exact_extension_of(&self, prior: &Self) -> Result<()> {
        self.validate()?;
        prior.validate()?;
        if self.records.len()
            != prior
                .records
                .len()
                .checked_add(1)
                .ok_or_else(|| invalid("history overflow"))?
            || self.records[..prior.records.len()] != prior.records
        {
            return Err(invalid("immutable history prefix"));
        }
        Ok(())
    }
}

fn validate_transition(
    prior: &SyncRemoteIntentRecordV2,
    next: &SyncRemoteIntentRecordV2,
) -> Result<()> {
    if next.identity != prior.identity
        || prior.sequence.checked_add(1) != Some(next.sequence)
        || next.previous_record_hash != prior.record_hash
    {
        return Err(invalid("chain"));
    }
    match (&prior.state, &next.state) {
        (
            SyncRemoteIntentStateV2::LocalPrepared { .. },
            SyncRemoteIntentStateV2::BeforeSend { .. },
        ) => {}
        (
            SyncRemoteIntentStateV2::LocalPrepared { prepared },
            SyncRemoteIntentStateV2::OutcomeUnknown { unknown },
        ) if unknown.previous_state == SyncPreviousStateV2::LocalPrepared => {
            if let Some(proof) = &unknown.created_commit_proof {
                proof.validate(&prior.identity, &prepared.prior_head)?;
                if unknown
                    .intended_commit
                    .as_ref()
                    .is_some_and(|oid| *oid != proof.commit)
                {
                    return Err(invalid("created intended commit"));
                }
            }
        }
        (
            SyncRemoteIntentStateV2::BeforeSend { before_send },
            SyncRemoteIntentStateV2::OutcomeUnknown { unknown },
        ) if unknown.previous_state == SyncPreviousStateV2::BeforeSend
            && unknown.created_commit_proof.is_none()
            && unknown.intended_commit.as_ref() == Some(&before_send.intended_commit) => {}
        (
            SyncRemoteIntentStateV2::BeforeSend { before_send },
            SyncRemoteIntentStateV2::Completed { receipt },
        ) if before_send.intended_commit == receipt.intended_commit => {}
        (
            SyncRemoteIntentStateV2::OutcomeUnknown { unknown },
            SyncRemoteIntentStateV2::Completed { receipt },
        ) if unknown
            .intended_commit
            .as_ref()
            .is_none_or(|oid| *oid == receipt.intended_commit)
            && unknown
                .created_commit_proof
                .as_ref()
                .is_none_or(|proof| proof.commit == receipt.intended_commit) => {}
        (
            SyncRemoteIntentStateV2::OutcomeUnknown { unknown },
            SyncRemoteIntentStateV2::ReconciledAbandon { evidence },
        ) => {
            let SyncAbandonEvidenceV2::RestoredLocal { evidence } = evidence.as_ref() else {
                return Err(invalid("abandon transition"));
            };
            if unknown.last_observation != evidence.last_observation {
                return Err(invalid("abandon observation"));
            }
            match unknown.previous_state {
                SyncPreviousStateV2::BeforeSend => {
                    if evidence.created_commit.as_ref() != unknown.intended_commit.as_ref()
                        || unknown.intended_commit.as_ref().is_none_or(|oid| {
                            evidence.expected_current_head
                                != (SyncPriorHeadV2::Present {
                                    commit: oid.clone(),
                                })
                        })
                    {
                        return Err(invalid("restore intended commit"));
                    }
                }
                SyncPreviousStateV2::LocalPrepared => match &unknown.created_commit_proof {
                    Some(proof)
                        if evidence.created_commit.as_ref() == Some(&proof.commit)
                            && evidence.expected_current_head
                                == (SyncPriorHeadV2::Present {
                                    commit: proof.commit.clone(),
                                }) => {}
                    None if evidence.created_commit.is_none()
                        && evidence.expected_current_head == evidence.prior_head => {}
                    _ => return Err(invalid("restore created commit proof")),
                },
            }
        }
        _ => return Err(invalid("transition")),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::{
    c10_test_history, c10_test_identity, c10_test_no_intent, c10_test_oid, c10_test_prepared,
    c10_test_receipt,
};
