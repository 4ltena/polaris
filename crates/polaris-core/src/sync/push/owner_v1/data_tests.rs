use super::*;
use crate::sync::share::CanonicalHttpsRemoteV1;
use polaris_auth::git_credentials::{
    AuthenticationTypeV1, ExecutableIdentityV1, OptionalAttributeHashV1,
};
use std::str::FromStr;

// DTO-only fixture: these values confer no credential or native owner authority.
fn prepared() -> SyncLocalPreparedV2 {
    let oid = SyncGitObjectIdV1::parse("a".repeat(40)).unwrap();
    SyncLocalPreparedV2 {
        payload: SyncPayloadDescriptorV1::new(vec![SyncPayloadEntryV1 {
            path: "src/main.rs".into(),
            mode: SyncPayloadModeV1::Regular,
            bytes: 3,
            content_hash: SyncDigestV1::digest(b"abc"),
        }])
        .unwrap(),
        prior_head: SyncPriorHeadV2::Present {
            commit: oid.clone(),
        },
        head_tree: SyncPayloadDescriptorV1::new(vec![]).unwrap(),
        agreed_commit: Some(oid.clone()),
        expected_remote: ExpectedSyncRemoteOidV1::Present { oid },
        prior_index_digest: SyncDigestV1::digest(b"prior index"),
        prior_staging_digest: SyncDigestV1::digest(b"prior staging"),
    }
}

fn identity() -> SyncIntentIdentityV2 {
    let prepared = prepared();
    let remote =
        CanonicalHttpsRemoteV1::parse("https://example.invalid:443/team/project.git").unwrap();
    let executable = |name: &str| ExecutableIdentityV1 {
        path: if cfg!(windows) {
            PathBuf::from(format!("C:/disposable/fixture/{name}"))
        } else {
            PathBuf::from(format!("/disposable/fixture/{name}"))
        },
        device: 1,
        inode: 2,
        owner_uid: 1,
        mode: 0o700,
        sha256: "a".repeat(64),
    };
    let binding = GitCredentialBindingV1 {
        namespace: "macos-keychain.internet-password.v1".into(),
        credential_item_id: "1".repeat(64),
        credential_item_version: "2".repeat(64),
        remote: remote.clone(),
        account: "synthetic-account".into(),
        authentication_type: AuthenticationTypeV1::HttpBasic,
        security_domain: OptionalAttributeHashV1::Absent,
        access_group: OptionalAttributeHashV1::Absent,
        synchronizable: false,
        generation: 1,
        git: executable("git"),
        askpass: executable("askpass"),
    };
    SyncIntentIdentityV2 {
        operation_id: Operation::from_str(&format!("{:032x}", 1)).unwrap(),
        request_hash: zero_digest_v2(),
        project: SyncDigestV1::digest(b"synthetic project"),
        share_id: SyncShareIdV1::parse("share-c10-0001").unwrap(),
        remote,
        remote_ref: SyncRemoteRefV1::parse("refs/heads/polaris").unwrap(),
        idempotency_hash: zero_digest_v2(),
        account_binding: binding,
        permission_hash: SyncDigestV1::digest(b"remote permission"),
        payload: prepared.payload.clone(),
        prior_participation_digest: SyncDigestV1::digest(b"prior participation"),
        expected_remote_oid: prepared.expected_remote.clone(),
    }
    .bind_local_request(&prepared)
    .unwrap()
}

#[test]
fn push_data_spawn_error_categories_remain_distinct() {
    assert!(matches!(
        SyncPushErrorV1::from(C10RepositoryErrorV1::BeforeSpawn),
        SyncPushErrorV1::NetworkBeforeSpawn
    ));
    assert!(matches!(
        SyncPushErrorV1::from(C10RepositoryErrorV1::AfterSpawn),
        SyncPushErrorV1::NetworkAfterSpawn
    ));
    for error in [
        C10RepositoryErrorV1::Protection,
        C10RepositoryErrorV1::Unavailable,
        C10RepositoryErrorV1::Io(std::io::Error::other("synthetic")),
    ] {
        assert!(matches!(
            SyncPushErrorV1::from(error),
            SyncPushErrorV1::Protection
        ));
    }
    assert!(matches!(
        SyncPushErrorV1::from(TransportFailureV1::Child(NetworkFailureV1::BeforeSpawn)),
        SyncPushErrorV1::NetworkBeforeSpawn
    ));
    for error in [
        TransportFailureV1::Child(NetworkFailureV1::AfterSpawn),
        TransportFailureV1::AlreadyAttempted,
        TransportFailureV1::ObjectFormatMismatch,
        TransportFailureV1::AncestryRejected,
        TransportFailureV1::AncestryUnavailable,
        TransportFailureV1::PushRejected,
        TransportFailureV1::InvalidObservation,
        TransportFailureV1::RecoveryFetchRejected,
    ] {
        assert!(matches!(
            SyncPushErrorV1::from(error),
            SyncPushErrorV1::NetworkAfterSpawn
        ));
    }
}

#[test]
fn push_data_request_and_canonical_history_preserve_identity() {
    let identity = identity();
    let history = SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared()).unwrap();
    let encoded = history.canonical_bytes().unwrap();
    let decoded = SyncRemoteIntentHistoryV2::from_canonical_bytes(&encoded).unwrap();
    assert_eq!(decoded, history);
    let request = request(&identity, 7).unwrap();
    assert_eq!(request.operation_id, identity.operation_id);
    assert_eq!(request.inventory.encoded(), "C10");
    assert_eq!(
        request.scope.project,
        management_hash(identity.project).unwrap()
    );
    assert_eq!(request.scope.session, None);
    assert_eq!(
        request.scope.generation,
        identity.account_binding.generation()
    );
    assert_eq!(request.scope.revision, 7);
    assert_eq!(
        request.scope.permission,
        ManagementPermissionScopeV1::Remote
    );
    assert_eq!(
        request.scope.account_binding,
        Some(identity.management_account_binding_hash().unwrap())
    );
    assert_eq!(
        request.request_hash,
        management_hash(identity.request_hash).unwrap()
    );
    assert_eq!(
        request.rewind,
        ManagementRewindDispositionV1::IrreversibleRemote
    );
    let mut effect = ManagementEffectSnapshotV1 {
        request,
        state: State::Prepared,
        last_sequence: 1,
    };
    validate_pair_identity(&decoded, &effect, identity.project).unwrap();
    // Pair identity deliberately uses the effect's revision, not an independent revision.
    effect.request.scope.revision = 8;
    validate_pair_identity(&decoded, &effect, identity.project).unwrap();
}

#[test]
fn push_data_pair_identity_rejects_each_mismatched_scope_field() {
    let identity = identity();
    let history = SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared()).unwrap();
    let original = ManagementEffectSnapshotV1 {
        request: request(&identity, 7).unwrap(),
        state: State::Prepared,
        last_sequence: 1,
    };
    validate_pair_identity(&history, &original, identity.project).unwrap();
    let wrong = management_hash(SyncDigestV1::digest(b"different")).unwrap();
    assert!(matches!(
        validate_pair_identity(&history, &original, SyncDigestV1::digest(b"different")),
        Err(SyncPushErrorV1::Protection)
    ));
    let changes: [fn(&mut PrepareManagementEffectV1, Hash); 9] = [
        |r, _| r.operation_id = Operation::from_str(&format!("{:032x}", 2)).unwrap(),
        |r, _| r.inventory = ManagementInventoryIdV1::parse("C08").unwrap(),
        |r, h| r.scope.project = h,
        |r, h| r.scope.session = Some(h),
        |r, _| r.scope.generation += 1,
        |r, _| r.scope.permission = ManagementPermissionScopeV1::Project,
        |r, _| r.scope.account_binding = None,
        |r, h| r.request_hash = h,
        |r, _| r.rewind = ManagementRewindDispositionV1::IrreversibleLocal,
    ];
    for change in changes {
        let mut effect = original.clone();
        change(&mut effect.request, wrong);
        assert!(matches!(
            validate_pair_identity(&history, &effect, identity.project),
            Err(SyncPushErrorV1::Protection)
        ));
    }
}
