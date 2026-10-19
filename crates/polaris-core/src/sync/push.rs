//! C10's shared push owner. Frontends consume its prepared token, never Git.
//! The state matrix preserves unsent HEAD and exact-ref reconciliation even
//! when the project payload already matches the local tree.

mod transport;

use super::remote_intent::{
    ExpectedSyncRemoteOidV1, SyncGitObjectIdV1, SyncPayloadDescriptorV1, SyncPriorHeadV2,
};

/// An immutable preview decision. Only the complete owner may execute it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPushActionV1 {
    NoChanges,
    PublishExistingHead,
    CreateCommit,
    ReconcileKnownHead,
}

fn push_action(
    payload: &SyncPayloadDescriptorV1,
    head_tree: &SyncPayloadDescriptorV1,
    head: &SyncPriorHeadV2,
    agreed: Option<&SyncGitObjectIdV1>,
    remote: &ExpectedSyncRemoteOidV1,
) -> SyncPushActionV1 {
    let SyncPriorHeadV2::Present { commit: head } = head else {
        // There is no existing commit to publish or use as a receipt.
        return SyncPushActionV1::CreateCommit;
    };
    if payload != head_tree {
        return SyncPushActionV1::CreateCommit;
    }
    if matches!(remote, ExpectedSyncRemoteOidV1::Present { oid } if oid == head) {
        if agreed == Some(head) {
            SyncPushActionV1::NoChanges
        } else {
            SyncPushActionV1::ReconcileKnownHead
        }
    } else {
        SyncPushActionV1::PublishExistingHead
    }
}

/// This is the subject of the fixed confirmation policy, not a
/// confirmation grant. All other binding attributes remain bound by the
/// complete intent/management request.
fn confirmation_permission_hash(
    operation: crate::management_effect::ManagementOperationIdV1,
    project: &super::remote_intent::SyncDigestV1,
    share: &super::remote_intent::SyncShareIdV1,
    remote: &polaris_auth::git_credentials::CanonicalHttpsRemoteV1,
    binding: &polaris_auth::git_credentials::GitCredentialBindingV1,
) -> Result<super::remote_intent::SyncDigestV1, super::remote_intent::SyncIntentValueErrorV2> {
    use sha2::{Digest, Sha256};
    binding
        .validate()
        .map_err(|_| super::remote_intent::SyncIntentValueErrorV2("permission binding"))?;
    if !binding.matches_remote(remote) {
        return Err(super::remote_intent::SyncIntentValueErrorV2(
            "permission remote",
        ));
    }
    let subject = serde_json::to_vec(&(
        project,
        share,
        remote,
        &binding.credential_item_id,
        binding.generation(),
    ))
    .map_err(|_| super::remote_intent::SyncIntentValueErrorV2("permission subject"))?;
    let mut hash = Sha256::new();
    hash.update(b"polaris-c10-confirmation-permission-v1\0");
    hash.update(operation.as_hex().as_bytes());
    hash.update(subject);
    hash.update(b"RequireExactPreparedConfirmation");
    super::remote_intent::SyncDigestV1::from_hex(&format!("{:x}", hash.finalize()))
        .map_err(|_| super::remote_intent::SyncIntentValueErrorV2("permission digest"))
}

/// Proof of a fresh exact-ref observation followed by one successful fetch.
/// Only this owner can mint it. Repository recovery consumes it and repeats
/// object/ref/descriptor durability before changing the primary local ref.
pub(crate) struct C10ExactRecoveryFetchV1 {
    identity: super::remote_intent::SyncIntentIdentityV2,
    intended: SyncGitObjectIdV1,
}
impl C10ExactRecoveryFetchV1 {
    pub(crate) fn identity(&self) -> &super::remote_intent::SyncIntentIdentityV2 {
        &self.identity
    }
    pub(crate) fn intended(&self) -> &SyncGitObjectIdV1 {
        &self.intended
    }
    pub(crate) fn recovery_ref(&self) -> String {
        format!(
            "refs/polaris/recovery/{}",
            super::remote_intent::SyncDigestV1::digest(
                self.identity.operation_id.as_hex().as_bytes()
            )
            .as_hex()
        )
    }
}

#[cfg(test)]
pub(crate) fn c10_test_exact_recovery_fetch(
    identity: super::remote_intent::SyncIntentIdentityV2,
    intended: SyncGitObjectIdV1,
) -> C10ExactRecoveryFetchV1 {
    C10ExactRecoveryFetchV1 { identity, intended }
}

mod owner_v1;
pub(crate) use owner_v1::{C10CompletedParticipationRecoveryV1, validate_pair_identity};
pub use owner_v1::{
    PreparedSyncPushV1, SyncPushConfirmationSummaryV1, SyncPushErrorV1, SyncPushHostV1,
    SyncPushOutcomeV1, SyncPushOwnerV1, SyncPushPreviewV1,
};

#[cfg(test)]
mod tests;
