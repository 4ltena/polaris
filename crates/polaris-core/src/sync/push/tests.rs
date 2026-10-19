//! Push payload decisions, exact confirmation and permission regression tests.
use super::*;
use crate::sync::remote_intent::{SyncDigestV1, SyncPayloadEntryV1, SyncPayloadModeV1};

fn oid(byte: &str) -> SyncGitObjectIdV1 {
    SyncGitObjectIdV1::parse(byte.repeat(40)).unwrap()
}
fn descriptor(bytes: &[u8]) -> SyncPayloadDescriptorV1 {
    SyncPayloadDescriptorV1::new(vec![SyncPayloadEntryV1 {
        path: "shared.txt".into(),
        mode: SyncPayloadModeV1::Regular,
        bytes: bytes.len() as u64,
        content_hash: SyncDigestV1::digest(bytes),
    }])
    .unwrap()
}

#[test]
fn unchanged_payload_still_preserves_unsent_deleted_and_rewound_remote_cases() {
    let payload = descriptor(b"current");
    let local = oid("1");
    let earlier = oid("2");
    let head = SyncPriorHeadV2::Present {
        commit: local.clone(),
    };
    let cases = [
        (
            Some(&local),
            ExpectedSyncRemoteOidV1::Present { oid: local.clone() },
            SyncPushActionV1::NoChanges,
        ),
        (
            Some(&earlier),
            ExpectedSyncRemoteOidV1::Present {
                oid: earlier.clone(),
            },
            SyncPushActionV1::PublishExistingHead,
        ),
        (
            Some(&local),
            ExpectedSyncRemoteOidV1::Missing,
            SyncPushActionV1::PublishExistingHead,
        ),
        (
            Some(&local),
            ExpectedSyncRemoteOidV1::Present {
                oid: earlier.clone(),
            },
            SyncPushActionV1::PublishExistingHead,
        ),
        (
            Some(&earlier),
            ExpectedSyncRemoteOidV1::Present { oid: local.clone() },
            SyncPushActionV1::ReconcileKnownHead,
        ),
        (
            None,
            ExpectedSyncRemoteOidV1::Present { oid: local.clone() },
            SyncPushActionV1::ReconcileKnownHead,
        ),
    ];
    for (agreed, remote, expected) in cases {
        assert_eq!(
            push_action(&payload, &payload, &head, agreed, &remote),
            expected
        );
    }
}

#[test]
fn new_payload_and_unborn_head_require_a_commit_even_with_matching_remote() {
    let old = descriptor(b"old");
    let new = descriptor(b"new");
    let local = oid("1");
    let remote = ExpectedSyncRemoteOidV1::Present { oid: local.clone() };
    assert_eq!(
        push_action(
            &new,
            &old,
            &SyncPriorHeadV2::Present {
                commit: local.clone()
            },
            Some(&local),
            &remote
        ),
        SyncPushActionV1::CreateCommit,
    );
    let empty = SyncPayloadDescriptorV1::new(Vec::new()).unwrap();
    assert_eq!(
        push_action(
            &empty,
            &empty,
            &SyncPriorHeadV2::Unborn,
            None,
            &ExpectedSyncRemoteOidV1::Missing
        ),
        SyncPushActionV1::CreateCommit,
    );
}

#[test]
fn confirmation_permission_binds_operation_and_exact_stable_subject() {
    use crate::sync::remote_intent::c10_test_identity;
    let identity = c10_test_identity(1);
    let compute = |value: &crate::sync::remote_intent::SyncIntentIdentityV2| {
        confirmation_permission_hash(
            value.operation_id,
            &value.project,
            &value.share_id,
            &value.remote,
            &value.account_binding,
        )
        .unwrap()
    };
    let expected = compute(&identity);
    assert_eq!(expected, compute(&identity));
    assert_eq!(
        expected.as_hex(),
        "76d8be9304410f718453405909b3e5b81bd8aaad1791c960494c63c0abbbf9f0"
    );
    assert_ne!(expected, crate::sync::remote_intent::zero_digest_v2());
    for kind in 0..6 {
        let mut different = identity.clone();
        match kind {
            0 => different.operation_id = c10_test_identity(2).operation_id,
            1 => different.project = SyncDigestV1::digest(b"another project"),
            2 => {
                different.share_id =
                    crate::sync::remote_intent::SyncShareIdV1::parse("share-c10-other").unwrap()
            }
            3 => different.account_binding.credential_item_id = "3".repeat(64),
            4 => different.account_binding.generation += 1,
            5 => {
                different.remote = polaris_auth::git_credentials::CanonicalHttpsRemoteV1::parse(
                    "https://other.invalid:443/team/project.git",
                )
                .unwrap();
                different.account_binding.remote = different.remote.clone();
            }
            _ => unreachable!(),
        }
        assert_ne!(expected, compute(&different));
    }
    // Full request binding separately retains item version and attributes;
    // invocation channel/nonce are never part of this stable subject.
    let mut rotated = identity.clone();
    rotated.account_binding.credential_item_version = "4".repeat(64);
    assert_eq!(expected, compute(&rotated));
}

#[test]
fn confirmation_permission_rejects_invalid_or_foreign_binding_subject() {
    use crate::sync::remote_intent::c10_test_identity;
    let mut identity = c10_test_identity(1);
    identity.account_binding.generation = 0;
    assert!(
        confirmation_permission_hash(
            identity.operation_id,
            &identity.project,
            &identity.share_id,
            &identity.remote,
            &identity.account_binding,
        )
        .is_err()
    );
    identity.account_binding.generation = 1;
    let foreign = polaris_auth::git_credentials::CanonicalHttpsRemoteV1::parse(
        "https://other.invalid:443/team/project.git",
    )
    .unwrap();
    assert!(
        confirmation_permission_hash(
            identity.operation_id,
            &identity.project,
            &identity.share_id,
            &foreign,
            &identity.account_binding,
        )
        .is_err()
    );
}
