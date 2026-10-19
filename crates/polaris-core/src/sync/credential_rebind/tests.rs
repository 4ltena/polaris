//! Disposable C08 owner tests. No HOME override, Keychain query or Git process.
use super::*;
use crate::management_effect::ManagementRewindCrossingV1;
use crate::sync::participation::Participation;
use crate::sync::share::{ShareLink, ShareManifest};
use polaris_auth::git_credentials::{
    AuthenticationTypeV1, ExecutableIdentityV1, OptionalAttributeHashV1,
};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

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

/// Pure factories used by the bounded storage tests, including 1024-record
/// capacity fixtures. They never resolve physical executable identities.
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
    let stored = C08StoredRecordV1::NoIntentAbandon {
        evidence: Box::new(evidence),
    };
    stored.canonical_bytes().unwrap();
    stored
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn empty() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let state = temporary.path().join("state");
        for directory in [&project, &state] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir(project.join(".polaris")).unwrap();
        fs::set_permissions(project.join(".polaris"), fs::Permissions::from_mode(0o700)).unwrap();
        let project = fs::canonicalize(project).unwrap();
        let state = fs::canonicalize(state).unwrap();
        crate::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        Self {
            _root: temporary,
            project,
            state,
        }
    }
    fn legacy() -> Self {
        let fixture = Self::empty();
        fixture.install_legacy(&legacy());
        fixture
    }
    fn participation_path(&self) -> PathBuf {
        self.state.join("sync").join(participation::FILE)
    }
    fn install_bytes(&self, bytes: &[u8]) {
        let sync = self.state.join("sync");
        if !sync.exists() {
            fs::create_dir(&sync).unwrap();
            fs::set_permissions(&sync, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(self.participation_path())
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn install_legacy(&self, value: &Participation) {
        value.validate().unwrap();
        self.install_bytes(&serde_json::to_vec(value).unwrap());
    }
    fn install_v2(&self, value: &ParticipationV2) {
        self.install_bytes(&value.canonical_bytes().unwrap());
    }
    fn try_owner(&self, valid: bool) -> Result<CredentialRebindOwnerV1> {
        let lock = ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace"))?;
        let lease = lock.try_mutation()?;
        Ok(CredentialRebindOwnerV1 {
            project_root: self.project.clone(),
            state_dir: self.state.clone(),
            lease,
            credentials: BindingVerifier::Test(valid),
            intents: C08IntentStoreV1::inspect(&self.state)?,
            stop_at: None,
        })
    }
    fn owner(&self, valid: bool) -> CredentialRebindOwnerV1 {
        self.try_owner(valid).unwrap()
    }
    fn inventory(&self) -> BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)> {
        fn walk(path: &Path, out: &mut BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            let bytes = if metadata.is_file() {
                fs::read(path).unwrap()
            } else {
                Vec::new()
            };
            out.insert(
                path.to_owned(),
                (metadata.mode(), metadata.ino(), metadata.nlink(), bytes),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), out);
                }
            }
        }
        let mut result = BTreeMap::new();
        walk(&self.state, &mut result);
        walk(&self.project, &mut result);
        result
    }
    fn effect(&self, id: Operation) -> ManagementEffectSnapshotV1 {
        let owner = self.owner(true);
        let inspected = ManagementEffectStoreV1::inspect_open_at_state_dir(
            &self.project,
            &self.state,
            &owner.lease,
        )
        .unwrap();
        match inspected {
            ManagementEffectInspectOpenV1::Present(value) => value
                .effects()
                .find(|e| e.request.operation_id == id)
                .unwrap()
                .clone(),
            _ => panic!("expected durable effect"),
        }
    }
    fn assert_completed(&self, preview: &CredentialRebindPreviewV1) {
        let effect = self.effect(preview.operation_id);
        assert!(matches!(effect.state, State::Completed { .. }));
        assert_eq!(
            participation::read_snapshot(&self.state)
                .unwrap()
                .exact_digest,
            Some(target_digest(&preview.next).unwrap())
        );
        let owner = self.owner(true);
        let record = owner.intents.lookup(preview.operation_id).unwrap();
        assert!(record.is_terminal());
        validate_pair(&effect, Some(record), owner.project_hash()).unwrap();
    }
    fn stopped(&self, stop: &'static str) -> CredentialRebindPreviewV1 {
        let mut owner = self.owner(true);
        let preview = owner.preview(binding(1)).unwrap();
        owner.stop_at = Some(stop);
        if stop == "prepared" {
            assert!(owner.prepare(preview.clone()).is_err());
        } else {
            assert!(owner.prepare(preview.clone()).unwrap().confirm().is_err());
        }
        preview
    }
}

#[test]
fn preview_and_preparation_refusal_preserve_entries_and_exact_participation() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let before = fixture.inventory();
    let preview = owner.preview(binding(1)).unwrap();
    assert_eq!(fixture.inventory(), before);
    assert_eq!(preview.next.inner(), &legacy());
    assert_eq!(preview.confirmation.previous_generation, None);
    assert_eq!(preview.confirmation.next_generation, 1);
    drop(owner);
    let owner = fixture.owner(true);
    let before = fixture.inventory();
    let mut changed = preview;
    changed.request_hash = Hash::digest(b"tampered proposal");
    assert!(owner.prepare(changed).is_err());
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn prepared_deny_and_token_drop_never_publish_participation() {
    for denied in [false, true] {
        let fixture = Fixture::legacy();
        let owner = fixture.owner(true);
        let preview = owner.preview(binding(1)).unwrap();
        let old = fs::read(fixture.participation_path()).unwrap();
        let token = owner.prepare(preview.clone()).unwrap();
        if denied {
            token.deny().unwrap();
        } else {
            drop(token);
        }
        assert_eq!(fs::read(fixture.participation_path()).unwrap(), old);
        assert!(!fixture.state.join("sync/credential-rebind-v1").exists());
        assert!(
            matches!(fixture.effect(preview.operation_id).state,
            State::Denied { .. } if denied)
                || (!denied && fixture.effect(preview.operation_id).state == State::Prepared)
        );
        let owner = fixture.owner(true);
        if !denied {
            let before = fixture.inventory();
            let next = owner.preview(binding(1)).unwrap();
            assert!(owner.prepare(next).is_err());
            assert_eq!(fixture.inventory(), before);
        }
    }
}

#[test]
fn exact_stale_preview_refuses_before_management_or_intent_write() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    let mut changed = legacy();
    changed.manifest.excluded.push("another-local".into());
    fixture.install_legacy(&changed);
    let before = fixture.inventory();
    assert!(owner.prepare(preview).is_err());
    assert_eq!(fixture.inventory(), before);
    assert!(!fixture.state.join("management-effects-v1").exists());
}

#[test]
fn project_lease_is_retained_by_prepared_token_and_released_on_deny() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let contender =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    assert!(matches!(
        contender.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    assert!(matches!(
        contender.try_activity(),
        Err(ProjectMutationError::Busy)
    ));
    let preview = owner.preview(binding(1)).unwrap();
    let token = owner.prepare(preview).unwrap();
    assert!(matches!(
        contender.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    token.deny().unwrap();
    assert!(contender.try_mutation().is_ok());
}

#[test]
fn checked_generation_preserves_every_participation_field_and_refuses_overflow() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    assert_eq!(preview.next.inner(), &legacy());
    drop(owner);
    let previous = ParticipationV2 {
        schema_version: 2,
        participation: legacy(),
        credential_binding: binding(7),
    };
    fixture.install_v2(&previous);
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(8)).unwrap();
    assert_eq!(preview.next.inner(), previous.inner());
    assert_eq!(preview.confirmation.previous_generation, Some(7));
    assert_eq!(preview.confirmation.next_generation, 8);
    drop(owner);
    let previous = ParticipationV2 {
        schema_version: 2,
        participation: legacy(),
        credential_binding: binding(u64::MAX),
    };
    fixture.install_v2(&previous);
    let owner = fixture.owner(true);
    let before = fixture.inventory();
    assert!(owner.preview(binding(1)).is_err());
    assert_eq!(fixture.inventory(), before);
}

struct TestClock(Option<u64>);
impl TestClock {
    fn at(now: u64) -> Self {
        let previous = TEST_NOW.with(|value| value.replace(Some(now)));
        Self(previous)
    }
    fn set(&self, now: u64) {
        TEST_NOW.with(|value| value.set(Some(now)));
    }
}
impl Drop for TestClock {
    fn drop(&mut self) {
        TEST_NOW.with(|value| value.set(self.0));
    }
}

#[test]
fn strict_join_requires_missing_participation_and_canonical_https() {
    let _clock = TestClock::at(1_000);
    let fixture = Fixture::empty();
    let owner = fixture.owner(true);
    let source = legacy();
    let preview = owner
        .preview_join(
            binding(1),
            source.share.clone(),
            source.manifest.clone(),
            1_000,
        )
        .unwrap();
    assert_eq!(preview.old_digest, None);
    assert_eq!(preview.confirmation.operation, C08OperationKindV1::Join);
    owner.prepare(preview.clone()).unwrap().confirm().unwrap();
    fixture.assert_completed(&preview);
    assert!(matches!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .classification,
        ParticipationReadV2::StaleCredentialBindingOnly(_)
    ));
    let owner = fixture.owner(true);
    let before = fixture.inventory();
    assert!(
        owner
            .preview_join(
                binding(1),
                source.share.clone(),
                source.manifest.clone(),
                1_000
            )
            .is_err()
    );
    assert_eq!(fixture.inventory(), before);
    drop(owner);
    let fixture = Fixture::empty();
    let owner = fixture.owner(true);
    let mut file_share = source.share;
    file_share.remote = "file:///disposable/remote.git".into();
    let before = fixture.inventory();
    assert!(
        owner
            .preview_join(binding(1), file_share, ShareManifest::default(), 1_000)
            .is_err()
    );
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn management_record_and_byte_reservation_shortfalls_have_write_zero() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_MANAGEMENT_BUDGET.with(|value| value.set(None));
        }
    }
    for budget in [(16_384, 3, 65_536), (16_384, 4, 65_535)] {
        let fixture = Fixture::legacy();
        let owner = fixture.owner(true);
        let preview = owner.preview(binding(1)).unwrap();
        let before = fixture.inventory();
        let _reset = Reset;
        TEST_MANAGEMENT_BUDGET.with(|value| value.set(Some(budget)));
        assert!(owner.prepare(preview).is_err());
        assert_eq!(fixture.inventory(), before);
        assert!(!fixture.state.join("management-effects-v1").exists());
        TEST_MANAGEMENT_BUDGET.with(|value| value.set(None));
    }
}

#[test]
fn maximum_history_is_reserved_before_prepared_not_only_first_record() {
    let fixture = Fixture::legacy();
    let mut source = legacy();
    source.manifest.excluded = (0..400)
        .map(|n| format!("entry-{n:04}-{}", "x".repeat(80)))
        .collect();
    fixture.install_legacy(&source);
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    let first = owner.first_intent(&preview).unwrap();
    assert!(first.canonical_bytes().unwrap().len() < MAX_RECORD);
    let before = fixture.inventory();
    assert!(owner.prepare(preview).is_err());
    assert_eq!(fixture.inventory(), before);
    assert!(!fixture.state.join("management-effects-v1").exists());
}

#[test]
fn credential_drift_before_started_is_durable_denied_and_publication_zero() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    let old = fs::read(fixture.participation_path()).unwrap();
    let mut token = owner.prepare(preview.clone()).unwrap();
    token.owner.as_mut().unwrap().credentials = BindingVerifier::Test(false);
    assert!(token.confirm().is_err());
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::Denied { .. }
    ));
    assert_eq!(fs::read(fixture.participation_path()).unwrap(), old);
    assert!(!fixture.state.join("sync/credential-rebind-v1").exists());
}

#[test]
fn every_owner_stop_point_reopens_only_its_legal_recovery_pair() {
    for stop in [
        "prepared",
        "started",
        "before-write",
        "participation-published",
        "intent-completed",
        "management-completed",
    ] {
        let fixture = Fixture::legacy();
        let preview = fixture.stopped(stop);
        if stop == "prepared" || stop == "started" {
            let owner = fixture.owner(true);
            assert!(
                owner
                    .recovery_needs_selection(preview.operation_id)
                    .unwrap()
            );
            owner
                .abandon(preview.operation_id, Some(preview.clone()))
                .unwrap();
            assert!(matches!(
                fixture.effect(preview.operation_id).state,
                State::Denied { .. } | State::ReconciledAbandon { .. }
            ));
            assert_eq!(
                participation::read_snapshot(&fixture.state)
                    .unwrap()
                    .exact_digest,
                preview.old_digest
            );
        } else {
            fixture.owner(true).recover(preview.operation_id).unwrap();
            fixture.assert_completed(&preview);
        }
    }
}

#[test]
fn no_intent_abandon_requires_exact_reselection_and_mismatch_has_write_zero() {
    for stop in ["prepared", "started"] {
        let fixture = Fixture::legacy();
        let preview = fixture.stopped(stop);
        let before = fixture.inventory();
        assert!(fixture.owner(true).recover(preview.operation_id).is_err());
        assert_eq!(fixture.inventory(), before);
        assert!(
            fixture
                .owner(true)
                .abandon(preview.operation_id, None)
                .is_err()
        );
        assert_eq!(fixture.inventory(), before);
        let mut mismatched = preview.clone();
        mismatched.prospective_binding.credential_item_version = "3".repeat(64);
        mismatched.next.credential_binding = mismatched.prospective_binding.clone();
        assert!(
            fixture
                .owner(true)
                .abandon(preview.operation_id, Some(mismatched))
                .is_err()
        );
        assert_eq!(fixture.inventory(), before);
        fixture
            .owner(true)
            .abandon(preview.operation_id, Some(preview.clone()))
            .unwrap();
        let effect = fixture.effect(preview.operation_id);
        if stop == "prepared" {
            assert!(matches!(effect.state, State::Denied { .. }));
        } else {
            assert!(matches!(effect.state, State::ReconciledAbandon { .. }));
            let owner = fixture.owner(true);
            assert!(matches!(
                owner.intents.lookup(preview.operation_id),
                Some(C08StoredRecordV1::NoIntentAbandon { .. })
            ));
        }
    }
}

#[test]
fn no_intent_old_digest_change_cannot_be_abandoned_by_inference() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("started");
    let mut changed = legacy();
    changed.share.invitation_id = "another-invitation".into();
    fixture.install_legacy(&changed);
    let before = fixture.inventory();
    assert!(
        fixture
            .owner(true)
            .abandon(preview.operation_id, Some(preview))
            .is_err()
    );
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn old_digest_recovery_revalidates_binding_before_any_write() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("before-write");
    let before = fixture.inventory();
    assert!(fixture.owner(false).recover(preview.operation_id).is_err());
    assert_eq!(fixture.inventory(), before);
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::OutcomeUnknown { .. }
    ));
    fixture.owner(true).recover(preview.operation_id).unwrap();
    fixture.assert_completed(&preview);
}

#[test]
fn published_target_recovery_repairs_receipts_without_fresh_credentials_or_republication() {
    for stop in ["participation-published", "intent-completed"] {
        let fixture = Fixture::legacy();
        let preview = fixture.stopped(stop);
        let before = fixture.inventory();
        let participation_before: BTreeMap<_, _> = before
            .iter()
            .filter(|(path, _)| {
                path.starts_with(fixture.state.join("sync"))
                    && !path.starts_with(fixture.state.join("sync/credential-rebind-v1"))
            })
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect();
        fixture.owner(false).recover(preview.operation_id).unwrap();
        let after = fixture.inventory();
        let participation_after: BTreeMap<_, _> = after
            .iter()
            .filter(|(path, _)| {
                path.starts_with(fixture.state.join("sync"))
                    && !path.starts_with(fixture.state.join("sync/credential-rebind-v1"))
            })
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect();
        assert_eq!(participation_after, participation_before);
        fixture.assert_completed(&preview);
    }
}

#[test]
fn other_digest_is_a_readonly_blocker_for_recover_and_abandon() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("before-write");
    let mut changed = legacy();
    changed.manifest.excluded.push("other".into());
    fixture.install_legacy(&changed);
    let before = fixture.inventory();
    assert!(fixture.owner(true).recover(preview.operation_id).is_err());
    assert_eq!(fixture.inventory(), before);
    assert!(
        fixture
            .owner(true)
            .abandon(preview.operation_id, None)
            .is_err()
    );
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn old_nonterminal_explicit_abandon_retains_unknown_then_terminal_history() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("before-write");
    fixture
        .owner(true)
        .abandon(preview.operation_id, None)
        .unwrap();
    let effect = fixture.effect(preview.operation_id);
    assert!(matches!(effect.state, State::ReconciledAbandon { .. }));
    assert_eq!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .exact_digest,
        preview.old_digest
    );
    let owner = fixture.owner(true);
    let C08StoredRecordV1::IntentHistory { records } =
        owner.intents.lookup(preview.operation_id).unwrap()
    else {
        panic!("intent history required");
    };
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].state, C08IntentStateV1::BeforeWrite);
    assert_eq!(records[1].state, C08IntentStateV1::OutcomeUnknown);
    assert!(matches!(
        records[2].state,
        C08IntentStateV1::ReconciledAbandon { .. }
    ));
    assert_eq!(records[1].previous_hash, records[0].record_hash);
    assert_eq!(records[2].previous_hash, records[1].record_hash);
}

#[test]
fn orphan_and_prepared_with_intent_pairs_are_blockers_without_repair() {
    for prepared in [false, true] {
        let fixture = Fixture::legacy();
        let owner = fixture.owner(true);
        let preview = owner.preview(binding(1)).unwrap();
        let mut owner = if prepared {
            let mut token = owner.prepare(preview.clone()).unwrap();
            token.owner.take().unwrap()
        } else {
            owner
        };
        owner
            .intents
            .reserve_operation(preview.operation_id)
            .unwrap();
        let first = owner.first_intent(&preview).unwrap();
        owner.intents.publish(&first).unwrap();
        let before = fixture.inventory();
        assert!(
            owner
                .inspect_management(Some(preview.operation_id))
                .is_err()
        );
        drop(owner);
        assert!(fixture.owner(true).recover(preview.operation_id).is_err());
        assert_eq!(fixture.inventory(), before);
    }
}

#[test]
fn terminal_management_receipt_mismatch_cannot_repair_a_matching_target() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    let mut token = owner.prepare(preview.clone()).unwrap();
    let mut owner = token.owner.take().unwrap();
    let effects = owner.open_management().unwrap();
    let started = effects.start(&owner.lease, &token.prepared).unwrap();
    let mut record = owner.first_intent(&preview).unwrap();
    owner.intents.publish(&record).unwrap();
    let digest = participation::publish_v2(
        &owner.state_dir,
        &owner.lease,
        preview.old_digest,
        &preview.next,
    )
    .unwrap();
    record = record
        .append(C08IntentStateV1::Completed {
            participation_digest: digest,
        })
        .unwrap();
    owner.intents.publish(&record).unwrap();
    effects
        .complete_started(
            &owner.lease,
            &started,
            Hash::digest(b"wrong stored receipt"),
        )
        .unwrap();
    drop(effects);
    drop(owner);
    let before = fixture.inventory();
    assert!(fixture.owner(true).recover(preview.operation_id).is_err());
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn foreign_remote_intent_and_unknown_record_hash_fail_before_management_recovery() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("before-write");
    fs::create_dir(fixture.state.join("sync/remote-intents-v1")).unwrap();
    let before = fixture.inventory();
    assert!(fixture.owner(true).recover(preview.operation_id).is_err());
    assert_eq!(fixture.inventory(), before);
    fs::remove_dir(fixture.state.join("sync/remote-intents-v1")).unwrap();
    let path = fixture
        .state
        .join("sync/credential-rebind-v1")
        .join(format!("{}.json", preview.operation_id));
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["records"][0]["record_hash"] = serde_json::json!("f".repeat(64));
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let before = fixture.inventory();
    assert!(fixture.try_owner(true).is_err());
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn terminal_c08_with_session_none_blocks_rewind_crossing() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let before = ManagementEffectStoreV1::rewind_checkpoint_for_project_at_state_dir(
        &fixture.project,
        &fixture.state,
        &owner.lease,
    )
    .unwrap();
    let preview = owner.preview(binding(1)).unwrap();
    owner.prepare(preview.clone()).unwrap().confirm().unwrap();
    let owner = fixture.owner(true);
    let after = ManagementEffectStoreV1::rewind_checkpoint_for_project_at_state_dir(
        &fixture.project,
        &fixture.state,
        &owner.lease,
    )
    .unwrap();
    assert!(matches!(
        ManagementEffectStoreV1::verify_rewind_crossing_for_project_at_state_dir(
            &fixture.project,
            &fixture.state,
            &owner.lease,
            &before,
            &after,
            true
        )
        .unwrap(),
        ManagementRewindCrossingV1::Blocked { .. }
    ));
}

#[test]
fn no_intent_durable_evidence_restart_finishes_and_idempotent_abandon_has_write_zero() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("started");
    let mut owner = fixture.owner(true);
    owner.stop_at = Some("no-intent-evidence");
    assert!(
        owner
            .abandon(preview.operation_id, Some(preview.clone()))
            .is_err()
    );
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::OutcomeUnknown { .. }
    ));
    fixture
        .owner(true)
        .abandon(preview.operation_id, None)
        .unwrap();
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::ReconciledAbandon { .. }
    ));
    assert_eq!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .exact_digest,
        preview.old_digest
    );
    assert!(
        fixture
            .state
            .join("sync/credential-rebind-v1/archive")
            .join(format!("{}.no-intent.json", preview.operation_id))
            .exists()
    );
    let before = fixture.inventory();
    fixture
        .owner(false)
        .abandon(preview.operation_id, None)
        .unwrap();
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn legitimate_new_project_or_share_hash_cannot_replace_the_confirmed_scope() {
    for change_project in [false, true] {
        let fixture = Fixture::legacy();
        let owner = fixture.owner(true);
        let mut preview = owner.preview(binding(1)).unwrap();
        if change_project {
            preview.confirmation.project = Hash::digest(b"other-project");
        } else {
            preview.next.participation.share.share_id = "other-share".into();
            preview.confirmation.share_id = "other-share".into();
        }
        preview.request_hash = request_hash(
            preview.confirmation.project,
            preview.old_digest,
            &preview.next,
            &preview.confirmation,
        )
        .unwrap();
        let before = fixture.inventory();
        assert!(owner.prepare(preview).is_err());
        assert_eq!(fixture.inventory(), before);
        assert!(!fixture.state.join("management-effects-v1").exists());
    }
}

#[test]
fn published_target_cannot_be_abandoned_and_keeps_unknown_receipt_unchanged() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("participation-published");
    let before = fixture.inventory();
    assert!(
        fixture
            .owner(true)
            .abandon(preview.operation_id, None)
            .is_err()
    );
    assert_eq!(fixture.inventory(), before);
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::OutcomeUnknown { .. }
    ));
}

#[test]
fn completed_management_replay_archives_active_then_repeated_recover_is_readonly() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("management-completed");
    let name = format!("{}.json", preview.operation_id);
    let root = fixture.state.join("sync/credential-rebind-v1");
    assert!(root.join(&name).exists());
    assert!(!root.join("archive").join(&name).exists());
    fixture.owner(false).recover(preview.operation_id).unwrap();
    assert!(!root.join(&name).exists());
    assert!(root.join("archive").join(&name).exists());
    fixture.assert_completed(&preview);
    {
        let owner = fixture.owner(true);
        assert!(owner.intents.is_archived(preview.operation_id));
    }
    let before = fixture.inventory();
    fixture.owner(false).recover(preview.operation_id).unwrap();
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn reconciled_abandon_terminal_replay_archives_active_then_is_readonly() {
    let fixture = Fixture::legacy();
    let preview = fixture.stopped("before-write");
    // Model the stop after the durable management terminal and before the
    // owner's archive/readback. Both histories remain fully valid and paired.
    let mut owner = fixture.owner(true);
    owner
        .intents
        .reserve_operation(preview.operation_id)
        .unwrap();
    let first = owner.intents.lookup(preview.operation_id).unwrap().clone();
    let unknown_record = first.append(C08IntentStateV1::OutcomeUnknown).unwrap();
    owner.intents.publish(&unknown_record).unwrap();
    let terminal = unknown_record
        .append(C08IntentStateV1::ReconciledAbandon {
            evidence_hash: Hash::digest(b"explicit-user-confirmed-abandon-proof"),
        })
        .unwrap();
    owner.intents.publish(&terminal).unwrap();
    let effects = owner.open_management().unwrap();
    let unknown = effects
        .unknown_effect(preview.operation_id)
        .unwrap()
        .unwrap();
    effects
        .reconcile_abandon(&owner.lease, &unknown, terminal.evidence_hash().unwrap())
        .unwrap();
    drop(effects);
    drop(owner);
    let name = format!("{}.json", preview.operation_id);
    let root = fixture.state.join("sync/credential-rebind-v1");
    assert!(root.join(&name).exists());
    assert!(!root.join("archive").join(&name).exists());
    fixture
        .owner(false)
        .abandon(preview.operation_id, None)
        .unwrap();
    assert!(!root.join(&name).exists());
    assert!(root.join("archive").join(&name).exists());
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::ReconciledAbandon { .. }
    ));
    assert_eq!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .exact_digest,
        preview.old_digest
    );
    {
        let owner = fixture.owner(true);
        assert!(owner.intents.is_archived(preview.operation_id));
    }
    let before = fixture.inventory();
    fixture
        .owner(false)
        .abandon(preview.operation_id, None)
        .unwrap();
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn accepted_join_preview_cannot_prepare_after_live_invitation_expiration() {
    let clock = TestClock::at(1_000);
    let fixture = Fixture::empty();
    let owner = fixture.owner(true);
    let source = legacy();
    let preview = owner
        .preview_join(binding(1), source.share, source.manifest, 1_000)
        .unwrap();
    clock.set(2_000);
    let before = fixture.inventory();
    assert!(owner.prepare(preview).is_err());
    assert_eq!(fixture.inventory(), before);
    assert!(!fixture.state.join("management-effects-v1").exists());
    assert!(!fixture.state.join("sync/credential-rebind-v1").exists());
    assert_eq!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .classification,
        ParticipationReadV2::Missing
    );
}

#[test]
fn prepared_join_expiration_is_denied_before_started_or_participation_publication() {
    let clock = TestClock::at(1_000);
    let fixture = Fixture::empty();
    let owner = fixture.owner(true);
    let source = legacy();
    let preview = owner
        .preview_join(binding(1), source.share, source.manifest, 1_000)
        .unwrap();
    let token = owner.prepare(preview.clone()).unwrap();
    clock.set(2_000);
    assert!(token.confirm().is_err());
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::Denied { .. }
    ));
    assert!(!fixture.state.join("sync/credential-rebind-v1").exists());
    assert!(!fixture.participation_path().exists());
    assert_eq!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .classification,
        ParticipationReadV2::Missing
    );
    assert!(
        fs::read_dir(&fixture.project)
            .unwrap()
            .all(|entry| entry.unwrap().file_name() == ".polaris")
    );
}

#[test]
fn orphan_apply_active_or_staging_evidence_blocks_prepare_without_repair() {
    for active in [false, true] {
        let fixture = Fixture::legacy();
        let root = fixture.state.join("apply-intents-v1");
        for directory in [
            &root,
            &root.join("active"),
            &root.join("archive"),
            &root.join("staging"),
        ] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let orphan = if active {
            root.join("active").join("a".repeat(64))
        } else {
            root.join("staging")
                .join(format!(".{}.{}.tmp", "a".repeat(64), "b".repeat(24)))
        };
        fs::create_dir(&orphan).unwrap();
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o700)).unwrap();
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(orphan.join("g00.v1.json"))
            .unwrap();
        file.write_all(b"orphaned invalid apply evidence").unwrap();
        file.sync_all().unwrap();
        let owner = fixture.owner(true);
        let preview = owner.preview(binding(1)).unwrap();
        let before = fixture.inventory();
        assert!(owner.prepare(preview).is_err());
        assert_eq!(fixture.inventory(), before);
        assert!(!fixture.state.join("management-effects-v1").exists());
        assert!(!fixture.state.join("sync/credential-rebind-v1").exists());
    }
}

#[test]
fn expired_no_intent_join_reconstructs_original_timestamp_for_explicit_abandon_only() {
    let clock = TestClock::at(1_000);
    let fixture = Fixture::empty();
    let source = legacy();
    let mut owner = fixture.owner(true);
    let preview = owner
        .preview_join(
            binding(1),
            source.share.clone(),
            source.manifest.clone(),
            1_000,
        )
        .unwrap();
    owner.stop_at = Some("started");
    assert!(owner.prepare(preview.clone()).unwrap().confirm().is_err());
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::OutcomeUnknown { .. }
    ));
    assert!(!fixture.state.join("sync/credential-rebind-v1").exists());
    assert!(!fixture.participation_path().exists());
    let project_before: BTreeMap<_, _> = fixture
        .inventory()
        .into_iter()
        .filter(|(path, _)| path.starts_with(&fixture.project))
        .collect();
    clock.set(2_000);
    let owner = fixture.owner(true);
    let reconstructed = owner
        .preview_join(binding(1), source.share, source.manifest, 1_000)
        .unwrap();
    assert_eq!(reconstructed.next.inner().joined_unix_seconds, 1_000);
    assert_eq!(reconstructed.old_digest, None);
    assert_eq!(
        reconstructed.confirmation.operation,
        C08OperationKindV1::Join
    );
    assert_eq!(reconstructed.request_hash, preview.request_hash);
    owner
        .abandon(preview.operation_id, Some(reconstructed))
        .unwrap();
    assert!(matches!(
        fixture.effect(preview.operation_id).state,
        State::ReconciledAbandon { .. }
    ));
    assert_eq!(
        participation::read_snapshot(&fixture.state)
            .unwrap()
            .classification,
        ParticipationReadV2::Missing
    );
    assert!(!fixture.participation_path().exists());
    assert!(
        fixture
            .state
            .join("sync/credential-rebind-v1/archive")
            .join(format!("{}.no-intent.json", preview.operation_id))
            .exists()
    );
    let owner = fixture.owner(true);
    let C08StoredRecordV1::NoIntentAbandon { evidence } =
        owner.intents.lookup(preview.operation_id).unwrap()
    else {
        panic!("exact no-intent evidence required");
    };
    assert_eq!(evidence.old_digest, None);
    assert_eq!(evidence.current_digest, None);
    assert_eq!(evidence.next.inner().joined_unix_seconds, 1_000);
    let project_after: BTreeMap<_, _> = fixture
        .inventory()
        .into_iter()
        .filter(|(path, _)| path.starts_with(&fixture.project))
        .collect();
    assert_eq!(project_after, project_before);
    assert!(!fixture.project.join(".git").exists());
}

#[test]
fn c10_idle_read_seam_preserves_bytes_and_legacy_remote_gate() {
    let fixture = Fixture::empty();
    let sync = fixture.state.join("sync");
    fs::create_dir(&sync).unwrap();
    fs::set_permissions(&sync, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(sync.join("remote-intents-v2")).unwrap();
    fs::set_permissions(
        sync.join("remote-intents-v2"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mutation =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let lease = mutation.try_mutation().unwrap();
    let before = fixture.inventory();
    let mut read =
        C08IdleReadV1::inspect_at_state_dir(&fixture.project, &fixture.state, &lease).unwrap();
    let first = read.require_idle().unwrap();
    assert_eq!(read.require_idle().unwrap(), first);
    assert!(matches!(
        read.management(),
        ManagementEffectInspectOpenV1::Absent
    ));
    assert_eq!(fixture.inventory(), before);
    assert!(reject_legacy_remote_intents(&fixture.state).is_ok());
    fs::create_dir(sync.join("remote-intents-v1")).unwrap();
    assert!(reject_legacy_remote_intents(&fixture.state).is_err());
}

#[test]
fn c10_idle_read_seam_rejects_c08_prepared_without_intent_and_foreign_ex() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    drop(owner.prepare(preview).unwrap());
    let mutation =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let lease = mutation.try_mutation().unwrap();
    let before = fixture.inventory();
    assert!(matches!(
        C08IdleReadV1::inspect_at_state_dir(&fixture.project, &fixture.state, &lease),
        Err(CredentialRebindErrorV1::Blocked("C08 is not idle")),
    ));
    assert_eq!(fixture.inventory(), before);
    let foreign = Fixture::empty();
    let foreign_mutation =
        ProjectMutationOwnerV1::at_store_root(&foreign.state.join("rewind-workspace")).unwrap();
    let foreign_lease = foreign_mutation.try_mutation().unwrap();
    assert!(
        C08IdleReadV1::inspect_at_state_dir(&fixture.project, &fixture.state, &foreign_lease)
            .is_err()
    );
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn c10_idle_read_seam_accepts_exact_terminal_pair_and_rejects_late_orphan() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    owner.prepare(preview.clone()).unwrap().confirm().unwrap();
    fixture.assert_completed(&preview);
    let mutation =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let lease = mutation.try_mutation().unwrap();
    let before = fixture.inventory();
    let read =
        C08IdleReadV1::inspect_at_state_dir(&fixture.project, &fixture.state, &lease).unwrap();
    assert!(matches!(
        read.management(),
        ManagementEffectInspectOpenV1::Present(_)
    ));
    drop(read);
    assert_eq!(fixture.inventory(), before);
    // A terminal history still needs its exact management pair after archive.
    // Remove only the owned disposable management tree name by moving it.
    let management = fixture.state.join("management-effects-v1");
    let moved = fixture.state.join("owned-management-detached");
    if management.exists() {
        fs::rename(management, moved).unwrap();
    } else {
        panic!("fixture's management owner must exist");
    }
    let detached = fixture.inventory();
    assert!(C08IdleReadV1::inspect_at_state_dir(&fixture.project, &fixture.state, &lease).is_err());
    assert_eq!(fixture.inventory(), detached);
}

#[test]
fn c10_idle_retained_absent_read_rejects_later_management_publication() {
    let fixture = Fixture::legacy();
    let owner = fixture.owner(true);
    let preview = owner.preview(binding(1)).unwrap();
    drop(owner.prepare(preview).unwrap());
    let management = fixture.state.join("management-effects-v1");
    let detached = fixture.state.join("owned-prepared-management-detached");
    fs::rename(&management, &detached).unwrap();
    let mutation =
        ProjectMutationOwnerV1::at_store_root(&fixture.state.join("rewind-workspace")).unwrap();
    let lease = mutation.try_mutation().unwrap();
    let mut read =
        C08IdleReadV1::inspect_at_state_dir(&fixture.project, &fixture.state, &lease).unwrap();
    assert!(matches!(
        read.management(),
        ManagementEffectInspectOpenV1::Absent
    ));
    fs::rename(&detached, &management).unwrap();
    let published = fixture.inventory();
    assert!(matches!(
        read.require_idle(),
        Err(CredentialRebindErrorV1::Blocked(
            "management appeared during read"
        )),
    ));
    assert_eq!(fixture.inventory(), published);
}
