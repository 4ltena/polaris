//! Remote intent v2 records, completed receipts and history validation tests.
use super::*;
use polaris_auth::git_credentials::{
    AuthenticationTypeV1, ExecutableIdentityV1, OptionalAttributeHashV1,
};
use std::{path::PathBuf, str::FromStr};

pub(crate) fn c10_test_prepared() -> SyncLocalPreparedV2 {
    SyncLocalPreparedV2 {
        payload: SyncPayloadDescriptorV1::new(vec![SyncPayloadEntryV1 {
            path: "src/main.rs".into(),
            mode: SyncPayloadModeV1::Regular,
            bytes: 3,
            content_hash: SyncDigestV1::digest(b"abc"),
        }])
        .unwrap(),
        prior_head: SyncPriorHeadV2::Present {
            commit: c10_test_oid('a'),
        },
        head_tree: SyncPayloadDescriptorV1::new(vec![]).unwrap(),
        agreed_commit: Some(c10_test_oid('a')),
        expected_remote: ExpectedSyncRemoteOidV1::Present {
            oid: c10_test_oid('a'),
        },
        prior_index_digest: SyncDigestV1::digest(b"prior index"),
        prior_staging_digest: SyncDigestV1::digest(b"prior staging"),
    }
}
pub(crate) fn c10_test_oid(value: char) -> SyncGitObjectIdV1 {
    SyncGitObjectIdV1::parse(value.to_string().repeat(40)).unwrap()
}
pub(crate) fn c10_test_identity(number: u64) -> SyncIntentIdentityV2 {
    let prepared = c10_test_prepared();
    let remote =
        CanonicalHttpsRemoteV1::parse("https://example.invalid:443/team/project.git").unwrap();
    let executable = |name: &str| ExecutableIdentityV1 {
        path: if cfg!(windows) {
            PathBuf::from(format!("C:{name}"))
        } else {
            PathBuf::from(name)
        },
        device: 1,
        inode: 2,
        owner_uid: {
            #[cfg(unix)]
            {
                unsafe { libc::geteuid() }
            }
            #[cfg(not(unix))]
            {
                // DTO-only synthetic UID; no native identity evidence.
                1
            }
        },
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
        git: executable("/disposable/fixture/git"),
        askpass: executable("/disposable/fixture/askpass"),
    };
    SyncIntentIdentityV2 {
        operation_id: ManagementOperationIdV1::from_str(&format!("{number:032x}")).unwrap(),
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
pub(crate) fn c10_test_receipt(identity: &SyncIntentIdentityV2) -> SyncCompletedReceiptV2 {
    SyncCompletedReceiptV2 {
        identity: identity.clone(),
        intended_commit: c10_test_oid('b'),
        expected_remote: identity.expected_remote_oid.clone(),
        observed_oid: c10_test_oid('b'),
        payload: identity.payload.clone(),
        participation_digest: SyncDigestV1::digest(b"completed participation"),
        source: SyncReceiptSourceV2::ReconciledExactRef,
    }
}
pub(crate) fn c10_test_history(number: u64, terminal: bool) -> SyncRemoteIntentHistoryV2 {
    let identity = c10_test_identity(number);
    let history =
        SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), c10_test_prepared()).unwrap();
    if !terminal {
        return history;
    }
    history
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('b'),
            reopened_descriptor: identity.payload.clone(),
            expected_remote: identity.expected_remote_oid.clone(),
        })
        .unwrap()
        .completed(c10_test_receipt(&identity))
        .unwrap()
}
pub(crate) fn c10_test_unknown(previous_state: SyncPreviousStateV2) -> SyncOutcomeUnknownV2 {
    SyncOutcomeUnknownV2 {
        previous_state,
        created_commit_proof: None,
        intended_commit: (previous_state == SyncPreviousStateV2::BeforeSend)
            .then(|| c10_test_oid('b')),
        last_observation: SyncRemoteObservationV2::Missing,
        reason: SyncUnknownReasonV2::AbandonRequested,
    }
}

fn local_unknown_with_created(number: u64) -> SyncRemoteIntentHistoryV2 {
    let first = c10_test_history(number, false);
    let cap = c10_test_created_reopen_cap(
        first.current().identity(),
        &c10_test_prepared().prior_head,
        c10_test_oid('b'),
    );
    first
        .outcome_unknown_after_reopen(c10_test_unknown(SyncPreviousStateV2::LocalPrepared), cap)
        .unwrap()
}
pub(crate) fn c10_test_no_intent(
    number: u64,
    inventory: SyncDigestV1,
) -> NoIntentAbandonEvidenceV2 {
    let prepared = c10_test_prepared();
    NoIntentAbandonEvidenceV2 {
        identity: c10_test_identity(number),
        prior_head: prepared.prior_head.clone(),
        current_head: prepared.prior_head,
        agreed_commit: prepared.agreed_commit,
        prior_descriptor: prepared.head_tree.clone(),
        current_descriptor: prepared.head_tree,
        prior_index_digest: prepared.prior_index_digest,
        current_index_digest: prepared.prior_index_digest,
        prior_staging_digest: prepared.prior_staging_digest,
        current_staging_digest: prepared.prior_staging_digest,
        last_observation: SyncRemoteObservationV2::Missing,
        active_inventory_digest: inventory,
        confirmation: SyncAbandonConfirmationV2::ExplicitUserConfirmation,
    }
}
fn restored(number: u64) -> RestoredLocalAbandonEvidenceV2 {
    let prepared = c10_test_prepared();
    let identity = c10_test_identity(number);
    RestoredLocalAbandonEvidenceV2 {
        identity: identity.clone(),
        prior_head: prepared.prior_head.clone(),
        expected_current_head: SyncPriorHeadV2::Present {
            commit: c10_test_oid('b'),
        },
        created_commit: Some(c10_test_oid('b')),
        restored_head: prepared.prior_head,
        prior_descriptor: prepared.head_tree.clone(),
        reopened_descriptor: prepared.head_tree,
        prior_index_digest: prepared.prior_index_digest,
        restored_index_digest: prepared.prior_index_digest,
        prior_staging_digest: prepared.prior_staging_digest,
        restored_staging_digest: prepared.prior_staging_digest,
        durable_restore_digest: SyncDigestV1::digest(b"durable reopened clean restore"),
        last_observation: SyncRemoteObservationV2::Missing,
        participation_digest: identity.prior_participation_digest,
        confirmation: SyncAbandonConfirmationV2::ExplicitUserConfirmation,
    }
}

#[test]
fn c10_v2_approved_paths_retain_every_generation() {
    let first = c10_test_history(1, false);
    let before = first
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('b'),
            reopened_descriptor: first.current().identity().payload.clone(),
            expected_remote: first.current().identity().expected_remote_oid.clone(),
        })
        .unwrap();
    let unknown = before
        .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::BeforeSend))
        .unwrap();
    let completed = unknown
        .completed(c10_test_receipt(first.current().identity()))
        .unwrap();
    assert_eq!(completed.records().len(), 4);
    assert_eq!(&completed.records()[..3], unknown.records());
    assert_eq!(completed.current().sequence(), 4);
    assert_eq!(
        completed.current().previous_record_hash(),
        unknown.current().record_hash()
    );
    assert_eq!(
        SyncRemoteIntentHistoryV2::from_canonical_bytes(&completed.canonical_bytes().unwrap())
            .unwrap(),
        completed
    );
    assert!(
        local_unknown_with_created(1)
            .reconciled_abandon(restored(1))
            .is_ok()
    );
}

#[test]
fn c10_v2_forbidden_transitions_and_terminal_updates_are_rejected() {
    let first = c10_test_history(1, false);
    assert!(
        first
            .completed(c10_test_receipt(first.current().identity()))
            .is_err()
    );
    assert!(first.reconciled_abandon(restored(1)).is_err());
    let unknown = first
        .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::LocalPrepared))
        .unwrap();
    assert!(
        unknown
            .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::LocalPrepared))
            .is_err()
    );
    let completed = c10_test_history(1, true);
    assert!(
        completed
            .completed(c10_test_receipt(completed.current().identity()))
            .is_err()
    );
    assert!(
        completed
            .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::BeforeSend))
            .is_err()
    );
}

#[test]
fn c10_v2_unknown_requires_exact_previous_state_and_intended_commit() {
    let first = c10_test_history(1, false);
    assert!(
        first
            .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::BeforeSend))
            .is_err()
    );
    let before = c10_test_history(1, true).records[..2].to_vec();
    let history = SyncRemoteIntentHistoryV2 {
        schema_version: 2,
        records: before,
    };
    let mut unknown = c10_test_unknown(SyncPreviousStateV2::BeforeSend);
    unknown.intended_commit = None;
    assert!(history.outcome_unknown(unknown).is_err());
    let mut unknown = c10_test_unknown(SyncPreviousStateV2::BeforeSend);
    unknown.intended_commit = Some(c10_test_oid('c'));
    assert!(history.outcome_unknown(unknown).is_err());
}

#[test]
fn c10_v2_sequence_skip_overflow_and_previous_hash_are_corruption() {
    let history = c10_test_history(1, true);
    for mode in 0..3 {
        let mut invalid_history = history.clone();
        let record = &mut invalid_history.records[1];
        match mode {
            0 => record.sequence = 3,
            1 => record.sequence = u64::MAX,
            _ => record.previous_record_hash = SyncDigestV1::digest(b"wrong previous"),
        }
        record.record_hash = record.computed_hash().unwrap();
        assert!(invalid_history.validate().is_err());
    }
    let mut first = c10_test_history(1, false);
    first.records[0].sequence = u64::MAX;
    first.records[0].record_hash = first.records[0].computed_hash().unwrap();
    assert!(
        first
            .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::LocalPrepared))
            .is_err()
    );
}

#[test]
fn c10_v2_full_request_hash_binds_credentials_and_phar_snapshot() {
    let identity = c10_test_identity(1);
    let prepared = c10_test_prepared();
    for mode in 0..10 {
        let mut changed = identity.clone();
        let mut changed_prepared = prepared.clone();
        match mode {
            0 => changed.account_binding.credential_item_version = "3".repeat(64),
            1 => changed.account_binding.git.inode += 1,
            2 => changed.account_binding.account = "other".into(),
            3 => changed.account_binding.generation += 1,
            4 => changed.project = SyncDigestV1::digest(b"other project"),
            5 => changed.share_id = SyncShareIdV1::parse("other-share").unwrap(),
            6 => changed.prior_participation_digest = SyncDigestV1::digest(b"other participation"),
            7 => changed_prepared.prior_index_digest = SyncDigestV1::digest(b"other index"),
            8 => changed_prepared.agreed_commit = Some(c10_test_oid('c')),
            _ => changed_prepared.prior_staging_digest = SyncDigestV1::digest(b"other staging"),
        }
        assert!(
            SyncRemoteIntentHistoryV2::local_prepared(changed.clone(), changed_prepared.clone())
                .is_err()
        );
        let rebound = changed.bind_local_request(&changed_prepared).unwrap();
        assert_ne!(rebound.request_hash, identity.request_hash);
        assert_ne!(rebound.idempotency_hash, identity.idempotency_hash);
        assert!(SyncRemoteIntentHistoryV2::local_prepared(rebound, changed_prepared).is_ok());
    }
}

#[test]
fn c10_v2_full_receipt_rejects_changed_identity_target_account_payload() {
    let identity = c10_test_identity(1);
    let first = c10_test_history(1, false);
    let before = first
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('b'),
            reopened_descriptor: identity.payload.clone(),
            expected_remote: identity.expected_remote_oid.clone(),
        })
        .unwrap();
    for mode in 0..10 {
        let mut receipt = c10_test_receipt(&identity);
        match mode {
            0 => receipt.identity.operation_id = c10_test_identity(2).operation_id,
            1 => receipt.identity.request_hash = SyncDigestV1::digest(b"other request"),
            2 => receipt.identity.project = SyncDigestV1::digest(b"other project"),
            3 => receipt.identity.share_id = SyncShareIdV1::parse("other-share").unwrap(),
            4 => {
                receipt.identity.remote =
                    CanonicalHttpsRemoteV1::parse("https://elsewhere.invalid:443/team/project.git")
                        .unwrap()
            }
            5 => receipt.identity.account_binding.credential_item_version = "3".repeat(64),
            6 => receipt.observed_oid = c10_test_oid('c'),
            7 => receipt.expected_remote = ExpectedSyncRemoteOidV1::Missing,
            8 => receipt.payload = SyncPayloadDescriptorV1::new(vec![]).unwrap(),
            _ => receipt.participation_digest = zero_digest_v2(),
        }
        assert!(before.completed(receipt).is_err());
    }
}

#[test]
fn c10_v2_prefix_identity_is_immutable_even_after_rehashing() {
    let history = c10_test_history(1, true);
    let mut changed = history.clone();
    changed.records[1].identity.account_binding.askpass.inode += 1;
    changed.records[1].record_hash = changed.records[1].computed_hash().unwrap();
    changed.records[2].previous_record_hash = changed.records[1].record_hash;
    changed.records[2].record_hash = changed.records[2].computed_hash().unwrap();
    assert!(changed.validate().is_err());
    let mut other = c10_test_identity(1);
    other.account_binding.git.inode += 1;
    other = other.bind_local_request(&c10_test_prepared()).unwrap();
    let rewrite = SyncRemoteIntentHistoryV2::local_prepared(other.clone(), c10_test_prepared())
        .unwrap()
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('b'),
            reopened_descriptor: other.payload.clone(),
            expected_remote: other.expected_remote_oid.clone(),
        })
        .unwrap();
    assert!(
        rewrite
            .is_exact_extension_of(&c10_test_history(1, false))
            .is_err()
    );
}

#[test]
fn c10_v2_closed_json_rejects_missing_unknown_and_noncanonical_fields() {
    let history = c10_test_history(1, false);
    let bytes = history.canonical_bytes().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["records"][0]["identity"]
        .as_object_mut()
        .unwrap()
        .remove("prior_participation_digest");
    assert!(
        SyncRemoteIntentHistoryV2::from_canonical_bytes(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["records"][0]["state"]["prepared"]["invented"] = true.into();
    assert!(
        SyncRemoteIntentHistoryV2::from_canonical_bytes(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    let mut spaced = bytes;
    spaced.push(b'\n');
    assert!(SyncRemoteIntentHistoryV2::from_canonical_bytes(&spaced).is_err());
    assert!(
        SyncRemoteIntentHistoryV2::from_canonical_bytes(&vec![b' '; MAX_SYNC_INTENT_BYTES_V2 + 1])
            .is_err()
    );
}

#[test]
fn c10_v2_payload_rejects_traversal_duplicates_bad_digest_and_overflow() {
    for path in [
        "../outside",
        "/absolute",
        "a//b",
        "a/.git/config",
        "a\\b",
        "a\nb",
    ] {
        assert!(
            SyncPayloadDescriptorV1::new(vec![SyncPayloadEntryV1 {
                path: path.into(),
                mode: SyncPayloadModeV1::Regular,
                bytes: 1,
                content_hash: SyncDigestV1::digest(b"x")
            }])
            .is_err()
        );
    }
    let entry = SyncPayloadEntryV1 {
        path: "src/main.rs".into(),
        mode: SyncPayloadModeV1::Regular,
        bytes: 3,
        content_hash: SyncDigestV1::digest(b"abc"),
    };
    assert!(SyncPayloadDescriptorV1::new(vec![entry.clone(), entry]).is_err());
    let mut payload = c10_test_prepared().payload;
    payload.digest = SyncDigestV1::digest(b"wrong");
    let identity = c10_test_identity(1);
    let mut prepared = c10_test_prepared();
    prepared.payload = payload;
    assert!(SyncRemoteIntentHistoryV2::local_prepared(identity, prepared).is_err());
    let entries = vec![
        SyncPayloadEntryV1 {
            path: "a".into(),
            mode: SyncPayloadModeV1::Regular,
            bytes: u64::MAX,
            content_hash: SyncDigestV1::digest(b"x"),
        },
        SyncPayloadEntryV1 {
            path: "b".into(),
            mode: SyncPayloadModeV1::Regular,
            bytes: 1,
            content_hash: SyncDigestV1::digest(b"x"),
        },
    ];
    assert!(SyncPayloadDescriptorV1::new(entries).is_err());
}

#[test]
fn c10_v2_unborn_and_before_send_descriptor_are_exact() {
    let mut prepared = c10_test_prepared();
    prepared.prior_head = SyncPriorHeadV2::Unborn;
    let identity = c10_test_identity(1).bind_local_request(&prepared).unwrap();
    let history =
        SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared.clone()).unwrap();
    assert!(
        history
            .before_send(SyncBeforeSendV2 {
                intended_commit: c10_test_oid('b'),
                reopened_descriptor: SyncPayloadDescriptorV1::new(vec![]).unwrap(),
                expected_remote: identity.expected_remote_oid
            })
            .is_err()
    );
    prepared.head_tree = prepared.payload.clone();
    assert!(c10_test_identity(1).bind_local_request(&prepared).is_err());
}

#[test]
fn c10_v2_no_intent_evidence_requires_full_unchanged_prior_state() {
    let evidence = c10_test_no_intent(1, SyncDigestV1::digest(b"verified complete inventory"));
    assert!(
        SyncRemoteIntentHistoryV2::no_intent_abandon(evidence.clone())
            .unwrap()
            .is_terminal()
    );
    for mode in 0..6 {
        let mut changed = evidence.clone();
        match mode {
            0 => changed.current_head = SyncPriorHeadV2::Unborn,
            1 => changed.current_descriptor = changed.identity.payload.clone(),
            2 => changed.current_index_digest = SyncDigestV1::digest(b"changed index"),
            3 => changed.current_staging_digest = SyncDigestV1::digest(b"changed staging"),
            4 => changed.agreed_commit = Some(c10_test_oid('c')),
            _ => changed.active_inventory_digest = zero_digest_v2(),
        }
        assert!(SyncRemoteIntentHistoryV2::no_intent_abandon(changed).is_err());
    }
}

#[test]
fn c10_v2_restored_evidence_requires_reopen_and_original_prepared_identity() {
    let unknown = local_unknown_with_created(1);
    assert!(unknown.reconciled_abandon(restored(1)).is_ok());
    for mode in 0..6 {
        let mut evidence = restored(1);
        match mode {
            0 => evidence.restored_head = SyncPriorHeadV2::Unborn,
            1 => evidence.restored_index_digest = SyncDigestV1::digest(b"wrong index"),
            2 => {
                evidence.prior_staging_digest = SyncDigestV1::digest(b"other prior");
                evidence.restored_staging_digest = evidence.prior_staging_digest;
            }
            3 => evidence.durable_restore_digest = zero_digest_v2(),
            4 => evidence.last_observation = SyncRemoteObservationV2::Unavailable,
            _ => evidence.created_commit = Some(c10_test_oid('c')),
        }
        assert!(unknown.reconciled_abandon(evidence).is_err());
    }
}

#[test]
fn c10_v2_generation_count_and_hash_tampering_are_bounded() {
    let mut history = c10_test_history(1, true);
    history.records.push(history.current().clone());
    history.records.push(history.current().clone());
    assert!(history.validate().is_err());
    let mut history = c10_test_history(1, false);
    history.records[0].record_hash = zero_digest_v2();
    assert!(history.validate().is_err());
    let mut history = c10_test_history(1, false);
    history.records[0].previous_record_hash = SyncDigestV1::digest(b"not zero");
    history.records[0].record_hash = history.records[0].computed_hash().unwrap();
    assert!(history.validate().is_err());
}

fn payload_entries(count: usize, bytes: u64) -> Vec<SyncPayloadEntryV1> {
    (0..count)
        .map(|index| SyncPayloadEntryV1 {
            path: format!("files/{index:05}.txt"),
            mode: SyncPayloadModeV1::Regular,
            bytes,
            content_hash: SyncDigestV1::digest(format!("entry {index}").as_bytes()),
        })
        .collect()
}

#[test]
fn c10_v2_payload_4097_and_20000_entries_remain_compact_in_full_history() {
    for count in [4097, crate::sync::repository::MAX_FILES] {
        let entries = payload_entries(count, 1);
        let payload = SyncPayloadDescriptorV1::new(entries.clone()).unwrap();
        assert_eq!(payload.entry_count(), count);
        assert_eq!(payload.total_bytes(), count as u64);
        let mut changed = entries;
        changed[count - 1].content_hash = SyncDigestV1::digest(b"changed final entry");
        assert_ne!(
            payload.digest(),
            SyncPayloadDescriptorV1::new(changed).unwrap().digest()
        );
        let mut prepared = c10_test_prepared();
        prepared.payload = payload.clone();
        let mut identity = c10_test_identity(1);
        identity.payload = payload;
        let identity = identity.bind_local_request(&prepared).unwrap();
        let history = SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared)
            .unwrap()
            .before_send(SyncBeforeSendV2 {
                intended_commit: c10_test_oid('b'),
                reopened_descriptor: identity.payload.clone(),
                expected_remote: identity.expected_remote_oid.clone(),
            })
            .unwrap()
            .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::BeforeSend))
            .unwrap()
            .completed(c10_test_receipt(&identity))
            .unwrap();
        let bytes = history.canonical_bytes().unwrap();
        assert!(bytes.len() < MAX_SYNC_INTENT_BYTES_V2);
        assert!(!String::from_utf8_lossy(&bytes).contains("files/00000.txt"));
        assert_eq!(
            SyncRemoteIntentHistoryV2::from_canonical_bytes(&bytes).unwrap(),
            history
        );
    }
}

#[test]
fn c10_v2_payload_uses_repository_count_file_and_total_byte_bounds() {
    use crate::sync::repository::{MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES};
    assert!(SyncPayloadDescriptorV1::new(payload_entries(MAX_FILES + 1, 0)).is_err());
    assert!(SyncPayloadDescriptorV1::new(payload_entries(1, MAX_FILE_BYTES)).is_ok());
    assert!(SyncPayloadDescriptorV1::new(payload_entries(1, MAX_FILE_BYTES + 1)).is_err());
    let count = (MAX_TOTAL_BYTES / MAX_FILE_BYTES) as usize;
    let mut entries = payload_entries(count, MAX_FILE_BYTES);
    assert_eq!(
        SyncPayloadDescriptorV1::new(entries.clone())
            .unwrap()
            .total_bytes(),
        MAX_TOTAL_BYTES
    );
    entries.push(SyncPayloadEntryV1 {
        path: "zz-final".into(),
        mode: SyncPayloadModeV1::Regular,
        bytes: 1,
        content_hash: SyncDigestV1::digest(b"x"),
    });
    assert!(SyncPayloadDescriptorV1::new(entries).is_err());
}

#[test]
fn c10_v2_payload_aggregate_tampering_cannot_reuse_request_or_immutable_prefix() {
    let first = c10_test_history(1, false);
    for mode in 0..3 {
        let mut changed = first.clone();
        let SyncRemoteIntentStateV2::LocalPrepared { prepared } = &mut changed.records[0].state
        else {
            unreachable!()
        };
        let payload = &mut prepared.payload;
        match mode {
            0 => payload.entry_count += 1,
            1 => payload.total_bytes += 1,
            _ => payload.digest = SyncDigestV1::digest(b"forged aggregate commitment"),
        }
        let changed_payload = prepared.payload.clone();
        changed.records[0].identity.payload = changed_payload;
        changed.records[0].record_hash = changed.records[0].computed_hash().unwrap();
        assert!(changed.validate().is_err());
        let SyncRemoteIntentStateV2::LocalPrepared { prepared } = changed.records[0].state() else {
            unreachable!()
        };
        let prepared = prepared.as_ref().clone();
        let identity = changed.records[0]
            .identity
            .clone()
            .bind_local_request(&prepared)
            .unwrap();
        let rebound = SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared)
            .unwrap()
            .before_send(SyncBeforeSendV2 {
                intended_commit: c10_test_oid('b'),
                reopened_descriptor: identity.payload.clone(),
                expected_remote: identity.expected_remote_oid.clone(),
            })
            .unwrap();
        assert_ne!(
            identity.request_hash,
            first.current().identity().request_hash
        );
        assert!(rebound.is_exact_extension_of(&first).is_err());
    }
}

#[test]
fn c10_v2_before_send_restore_requires_the_exact_known_created_commit() {
    let first = c10_test_history(1, false);
    let unknown = first
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('b'),
            reopened_descriptor: first.current().identity().payload.clone(),
            expected_remote: first.current().identity().expected_remote_oid.clone(),
        })
        .unwrap()
        .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::BeforeSend))
        .unwrap();
    assert!(unknown.reconciled_abandon(restored(1)).is_ok());
    for created in [None, Some(c10_test_oid('c'))] {
        let mut evidence = restored(1);
        evidence.expected_current_head = created
            .clone()
            .map_or(evidence.prior_head.clone(), |commit| {
                SyncPriorHeadV2::Present { commit }
            });
        evidence.created_commit = created;
        assert!(unknown.reconciled_abandon(evidence).is_err());
    }
}

#[test]
fn c10_v2_local_prepared_without_created_proof_restores_exact_prior_head() {
    for prior_head in [c10_test_prepared().prior_head, SyncPriorHeadV2::Unborn] {
        let mut prepared = c10_test_prepared();
        prepared.prior_head = prior_head.clone();
        let identity = c10_test_identity(1).bind_local_request(&prepared).unwrap();
        let unknown = SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared)
            .unwrap()
            .outcome_unknown(c10_test_unknown(SyncPreviousStateV2::LocalPrepared))
            .unwrap();
        let mut evidence = restored(1);
        evidence.identity = identity;
        evidence.prior_head = prior_head.clone();
        evidence.restored_head = prior_head.clone();
        evidence.expected_current_head = prior_head;
        evidence.created_commit = None;
        assert!(unknown.reconciled_abandon(evidence.clone()).is_ok());
        evidence.expected_current_head = SyncPriorHeadV2::Present {
            commit: c10_test_oid('c'),
        };
        assert!(unknown.reconciled_abandon(evidence).is_err());
        assert!(unknown.reconciled_abandon(restored(1)).is_err());
    }
}

#[test]
fn c10_v2_created_reopen_capability_binds_operation_prior_payload_and_restored_commit() {
    let first = c10_test_history(1, false);
    let identity = first.current().identity();
    for mode in 0..5 {
        let mut cap = c10_test_created_reopen_cap(
            identity,
            &c10_test_prepared().prior_head,
            c10_test_oid('b'),
        );
        match mode {
            0 => cap.persisted.operation_id = c10_test_identity(2).operation_id,
            1 => cap.persisted.request_hash = SyncDigestV1::digest(b"another request"),
            2 => cap.persisted.prior_head = SyncPriorHeadV2::Unborn,
            3 => cap.persisted.reopened_descriptor = SyncPayloadDescriptorV1::new(vec![]).unwrap(),
            _ => {
                cap.persisted.reopened_objects_digest =
                    SyncDigestV1::digest(b"another reopen without matching domain evidence")
            }
        }
        if mode < 4 {
            cap.persisted.evidence_hash = cap.persisted.computed_hash().unwrap();
        }
        assert!(
            first
                .outcome_unknown_after_reopen(
                    c10_test_unknown(SyncPreviousStateV2::LocalPrepared),
                    cap
                )
                .is_err()
        );
    }
    let cap =
        c10_test_created_reopen_cap(identity, &c10_test_prepared().prior_head, c10_test_oid('b'));
    let mut raw = c10_test_unknown(SyncPreviousStateV2::LocalPrepared);
    raw.created_commit_proof = Some(cap.persisted);
    assert!(first.outcome_unknown(raw).is_err());
    let cap =
        c10_test_created_reopen_cap(identity, &c10_test_prepared().prior_head, c10_test_oid('b'));
    let mut wrong_intended = c10_test_unknown(SyncPreviousStateV2::LocalPrepared);
    wrong_intended.intended_commit = Some(c10_test_oid('c'));
    assert!(
        first
            .outcome_unknown_after_reopen(wrong_intended, cap)
            .is_err()
    );
    let unknown = local_unknown_with_created(1);
    assert!(unknown.reconciled_abandon(restored(1)).is_ok());
    let mut other = restored(1);
    other.created_commit = Some(c10_test_oid('c'));
    other.expected_current_head = SyncPriorHeadV2::Present {
        commit: c10_test_oid('c'),
    };
    assert!(unknown.reconciled_abandon(other).is_err());
    let mut receipt = c10_test_receipt(identity);
    receipt.intended_commit = c10_test_oid('c');
    receipt.observed_oid = c10_test_oid('c');
    assert!(unknown.completed(receipt).is_err());
    assert_eq!(
        SyncRemoteIntentHistoryV2::from_canonical_bytes(&unknown.canonical_bytes().unwrap())
            .unwrap(),
        unknown
    );
}
