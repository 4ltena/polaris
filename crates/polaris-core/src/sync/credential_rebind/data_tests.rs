//! Pure C08 DTO fixtures: no credential owner, live executable or project lease.
use super::*;
use crate::sync::participation::Participation;
use crate::sync::share::{ShareLink, ShareManifest};
use polaris_auth::git_credentials::{
    AuthenticationTypeV1, ExecutableIdentityV1, OptionalAttributeHashV1,
};
use std::str::FromStr;

fn operation(number: u64) -> Operation {
    Operation::from_str(&format!("{number:032x}")).unwrap()
}

#[cfg(unix)]
fn binding(generation: u64) -> GitCredentialBindingV1 {
    let remote =
        CanonicalHttpsRemoteV1::parse("https://example.invalid:443/team/project.git").unwrap();
    let executable = |name: &str| ExecutableIdentityV1 {
        path: PathBuf::from(name),
        device: 1,
        inode: 2,
        owner_uid: unsafe { libc::geteuid() },
        mode: 0o700,
        sha256: "a".repeat(64),
    };
    GitCredentialBindingV1 {
        namespace: "macos-keychain.internet-password.v1".into(),
        credential_item_id: "1".repeat(64),
        credential_item_version: "2".repeat(64),
        remote,
        account: "synthetic-account".into(),
        authentication_type: AuthenticationTypeV1::HttpBasic,
        security_domain: OptionalAttributeHashV1::Absent,
        access_group: OptionalAttributeHashV1::Absent,
        synchronizable: false,
        generation,
        git: executable("/disposable/fixture/git"),
        askpass: executable("/disposable/fixture/polaris-git-askpass"),
    }
}

#[cfg(not(unix))]
fn binding(generation: u64) -> GitCredentialBindingV1 {
    let remote =
        CanonicalHttpsRemoteV1::parse("https://example.invalid:443/team/project.git").unwrap();
    let executable = |name: &str| ExecutableIdentityV1 {
        path: PathBuf::from(name),
        device: 1,
        inode: 2,
        owner_uid: 1,
        mode: 0o700,
        sha256: "a".repeat(64),
    };
    GitCredentialBindingV1 {
        namespace: "macos-keychain.internet-password.v1".into(),
        credential_item_id: "1".repeat(64),
        credential_item_version: "2".repeat(64),
        remote,
        account: "synthetic-account".into(),
        authentication_type: AuthenticationTypeV1::HttpBasic,
        security_domain: OptionalAttributeHashV1::Absent,
        access_group: OptionalAttributeHashV1::Absent,
        synchronizable: false,
        generation,
        git: executable("C:/disposable/fixture/git"),
        askpass: executable("C:/disposable/fixture/polaris-git-askpass"),
    }
}

fn legacy() -> Participation {
    Participation {
        share: ShareLink {
            remote: binding(1).remote.transport_url(),
            share_id: "share-c08-0001".into(),
            invitation_id: "invite-c08-0001".into(),
            expires_unix_seconds: 2_000,
        },
        manifest: ShareManifest {
            version: 1,
            excluded: vec!["local-build".into()],
        },
        joined_unix_seconds: 1_000,
        agreed_commit: Some("a".repeat(40)),
        applied_commit: Some("a".repeat(40)),
        unresolved: vec!["src/preserved.rs".into()],
    }
}

fn pure_preview(id: Operation) -> CredentialRebindPreviewV1 {
    let source = legacy();
    let bytes = serde_json::to_vec(&source).unwrap();
    let snapshot = ParticipationSnapshotV2 {
        classification: ParticipationReadV2::LegacyParticipationOnly(source),
        exact_digest: Some(SyncDigestV1::digest(&bytes)),
    };
    proposal(
        Hash::digest(b"synthetic-c08-project"),
        &snapshot,
        binding(1),
        id,
    )
    .unwrap()
}

fn request_from_preview(preview: &CredentialRebindPreviewV1) -> PrepareManagementEffectV1 {
    PrepareManagementEffectV1 {
        operation_id: preview.operation_id,
        inventory: ManagementInventoryIdV1::parse("C08").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: preview.confirmation.project,
            session: None,
            generation: preview.prospective_binding.generation(),
            revision: 0,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: preview.request_hash,
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    }
}

fn first_from_preview(preview: &CredentialRebindPreviewV1) -> C08StoredRecordV1 {
    let mut intent = C08RebindIntentV1 {
        schema_version: 1,
        operation_id: preview.operation_id,
        request_hash: preview.request_hash,
        project: preview.confirmation.project,
        share_id: preview.confirmation.share_id.clone(),
        old_digest: preview.old_digest,
        next: preview.next.clone(),
        prospective_binding: preview.prospective_binding.clone(),
        confirmation: preview.confirmation.clone(),
        sequence: 1,
        previous_hash: zero_hash().unwrap(),
        state: C08IntentStateV1::BeforeWrite,
        record_hash: zero_hash().unwrap(),
    };
    intent.record_hash = intent.computed_hash().unwrap();
    let stored = C08StoredRecordV1::IntentHistory {
        records: vec![intent],
    };
    stored.canonical_bytes().unwrap();
    stored
}

pub(super) fn stored_record(id: Operation, terminal: bool) -> C08StoredRecordV1 {
    let record = first_from_preview(&pure_preview(id));
    if terminal {
        record
            .append(C08IntentStateV1::Completed {
                participation_digest: target_digest(&record.latest().unwrap().next).unwrap(),
            })
            .unwrap()
    } else {
        record
    }
}

pub(super) fn no_intent_record(id: Operation) -> C08StoredRecordV1 {
    let preview = pure_preview(id);
    let mut evidence = C08NoIntentAbandonEvidenceV1 {
        schema_version: 1,
        operation_id: id,
        management_request: request_from_preview(&preview),
        old_digest: preview.old_digest,
        current_digest: preview.old_digest,
        next: preview.next,
        prospective_binding: preview.prospective_binding,
        confirmation: preview.confirmation,
        c08_inventory_digest: Hash::digest(b"synthetic-complete-empty-inventory"),
        user_confirmed: true,
        evidence_hash: zero_hash().unwrap(),
    };
    evidence.evidence_hash = evidence.computed_hash().unwrap();
    let stored = C08StoredRecordV1::NoIntentAbandon { evidence };
    stored.canonical_bytes().unwrap();
    stored
}

#[test]
fn record_hash_chain_closed_states_and_terminal_immutability_are_enforced() {
    let first = stored_record(operation(1), false);
    let unknown = first.append(C08IntentStateV1::OutcomeUnknown).unwrap();
    let complete = unknown
        .append(C08IntentStateV1::Completed {
            participation_digest: target_digest(&unknown.latest().unwrap().next).unwrap(),
        })
        .unwrap();
    complete.is_extension_of(&unknown).unwrap();
    assert!(complete.append(C08IntentStateV1::OutcomeUnknown).is_err());
    assert!(
        first
            .append(C08IntentStateV1::ReconciledAbandon {
                evidence_hash: Hash::digest(b"proof")
            })
            .is_err()
    );
    for case in 0..4 {
        let mut broken = complete.clone();
        let C08StoredRecordV1::IntentHistory { records } = &mut broken else {
            unreachable!()
        };
        match case {
            0 => records[0].previous_hash = Hash::digest(b"not zero"),
            1 => records[1].sequence = 4,
            2 => records[1].previous_hash = Hash::digest(b"not prior"),
            3 => {
                records[1]
                    .next
                    .participation
                    .manifest
                    .excluded
                    .push("immutable-change".into());
                records[1].request_hash = request_hash(
                    records[1].project,
                    records[1].old_digest,
                    &records[1].next,
                    &records[1].confirmation,
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let index = if case == 0 { 0 } else { 1 };
        records[index].record_hash = records[index].computed_hash().unwrap();
        assert!(broken.canonical_bytes().is_err());
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(&first.canonical_bytes().unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("unknown".into(), serde_json::json!(true));
    assert!(serde_json::from_value::<C08StoredRecordV1>(value).is_err());
}

#[test]
fn request_hash_binds_exact_old_bytes_full_binding_and_confirmation() {
    let preview = pure_preview(operation(1));
    let original = preview.request_hash;
    let mut changed = preview.next.clone();
    changed.credential_binding.credential_item_version = "3".repeat(64);
    assert_ne!(
        request_hash(
            preview.confirmation.project,
            preview.old_digest,
            &changed,
            &preview.confirmation
        )
        .unwrap(),
        original
    );
    changed = preview.next.clone();
    changed.credential_binding.git.inode += 1;
    assert_ne!(
        request_hash(
            preview.confirmation.project,
            preview.old_digest,
            &changed,
            &preview.confirmation
        )
        .unwrap(),
        original
    );
    assert_ne!(
        request_hash(
            preview.confirmation.project,
            Some(SyncDigestV1::digest(b"whitespace differs")),
            &preview.next,
            &preview.confirmation
        )
        .unwrap(),
        original
    );
    let mut summary = preview.confirmation.clone();
    summary.operation = C08OperationKindV1::Join;
    assert!(request_hash(summary.project, preview.old_digest, &preview.next, &summary).is_err());
    let mut evidence = no_intent_record(operation(1));
    let C08StoredRecordV1::NoIntentAbandon { evidence: value } = &mut evidence else {
        unreachable!()
    };
    value.user_confirmed = false;
    value.evidence_hash = value.computed_hash().unwrap();
    assert!(evidence.canonical_bytes().is_err());
}

#[test]
fn preview_debug_redacts_remote_account_and_physical_executable_paths() {
    let preview = pure_preview(operation(1));
    let text = format!("{preview:?}");
    for forbidden in [
        "https://example.invalid:443/team/project.git",
        "synthetic-account",
        "/disposable/fixture/git",
    ] {
        assert!(!text.contains(forbidden));
    }
    let io = CredentialRebindErrorV1::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "sensitive-path",
    ));
    assert_eq!(io.to_string(), "c08_storage");
}

#[test]
fn verified_current_participation_cannot_use_legacy_stale_rebind_admission() {
    let next = ParticipationV2 {
        schema_version: 2,
        participation: legacy(),
        credential_binding: binding(1),
    };
    let snapshot = ParticipationSnapshotV2 {
        exact_digest: Some(SyncDigestV1::digest(&next.canonical_bytes().unwrap())),
        classification: ParticipationReadV2::Current(next),
    };
    assert!(
        proposal(
            Hash::digest(b"project"),
            &snapshot,
            binding(2),
            operation(1)
        )
        .is_err()
    );
}

#[test]
fn credential_rebind_dto_fixture_roundtrips_and_hides_both_executable_paths() {
    let preview = pure_preview(operation(7));
    preview.prospective_binding.validate().unwrap();
    assert_eq!(preview.prospective_binding.generation(), 1);
    assert_eq!(preview.confirmation.next_generation, 1);
    let record = stored_record(operation(7), false);
    let bytes = record.canonical_bytes().unwrap();
    let decoded: C08StoredRecordV1 = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
    assert_eq!(decoded.operation_id(), operation(7));
    let text = format!("{preview:?}");
    for identity in [
        &preview.prospective_binding.git,
        &preview.prospective_binding.askpass,
    ] {
        assert!(identity.path.is_absolute());
        assert!(!text.contains(identity.path.to_str().unwrap()));
    }
}
