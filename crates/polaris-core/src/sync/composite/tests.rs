//! Composite sync read ownership, intent history and recovery regression tests.
use super::*;
use crate::{
    management_effect::{
        ManagementEffectHashV1 as Hash, ManagementEffectScopeV1, ManagementEffectStoreV1,
        ManagementInventoryIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
        PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
    },
    sync::{
        participation::{Participation, ParticipationV2},
        remote_intent::*,
        share::{ShareLink, ShareManifest},
    },
};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

struct Fixture {
    _temp: tempfile::TempDir,
    project: PathBuf,
    state: PathBuf,
    owner: ProjectMutationOwnerV1,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let project = fs::canonicalize(project).unwrap();
        directory(&project.join(".polaris"));
        let state = temp.path().join("state");
        directory(&state);
        crate::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        Self {
            _temp: temp,
            project,
            state,
            owner,
        }
    }
    fn inventory(&self) -> BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)> {
        fn visit(path: &Path, values: &mut BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)>) {
            let m = fs::symlink_metadata(path).unwrap();
            values.insert(
                path.to_owned(),
                (
                    m.mode(),
                    m.ino(),
                    m.nlink(),
                    if m.is_file() {
                        fs::read(path).unwrap()
                    } else {
                        Vec::new()
                    },
                ),
            );
            if m.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(&entry.unwrap().path(), values);
                }
            }
        }
        let mut values = BTreeMap::new();
        visit(&self.state, &mut values);
        values
    }
    fn history(&self, number: u64) -> SyncRemoteIntentHistoryV2 {
        let mut identity = c10_test_identity(number);
        identity.project = SyncDigestV1::digest(self.project.as_os_str().as_encoded_bytes());
        let identity = identity.bind_local_request(&c10_test_prepared()).unwrap();
        SyncRemoteIntentHistoryV2::local_prepared(identity, c10_test_prepared()).unwrap()
    }
    fn install(&self, history: &SyncRemoteIntentHistoryV2, archived: bool) -> PathBuf {
        let root = self.state.join("sync/remote-intents-v2");
        for path in [self.state.join("sync"), root.clone(), root.join("archive")] {
            if !path.exists() {
                directory(&path);
            }
        }
        if !root.join("owner.lock").exists() {
            write(&root.join("owner.lock"), b"");
        }
        let name = format!(
            "{}.json",
            SyncDigestV1::digest(history.operation_id().as_hex().as_bytes()).as_hex()
        );
        let location = if archived {
            let bucket = root.join("archive").join(&name[..2]);
            if !bucket.exists() {
                directory(&bucket);
            }
            bucket
        } else {
            root
        };
        let path = location.join(name);
        write(&path, &history.canonical_bytes().unwrap());
        path
    }
    fn management(&self, lease: &ProjectMutationLeaseV1) -> ManagementEffectStoreV1 {
        ManagementEffectStoreV1::open_at_state_dir(&self.project, &self.state, lease).unwrap()
    }
    fn request(&self, history: &SyncRemoteIntentHistoryV2) -> PrepareManagementEffectV1 {
        let i = history.current().identity();
        PrepareManagementEffectV1 {
            operation_id: i.operation_id,
            inventory: ManagementInventoryIdV1::parse("C10").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: hash(i.project),
                session: None,
                generation: i.account_binding.generation(),
                revision: 0,
                permission: ManagementPermissionScopeV1::Remote,
                account_binding: Some(i.management_account_binding_hash().unwrap()),
            },
            request_hash: hash(i.request_hash),
            rewind: ManagementRewindDispositionV1::IrreversibleRemote,
        }
    }
    fn seed_started(&self, lease: &ProjectMutationLeaseV1, history: &SyncRemoteIntentHistoryV2) {
        let store = self.management(lease);
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = store
            .prepare(lease, store.cursor().unwrap(), self.request(history))
            .unwrap()
        else {
            panic!("fresh request");
        };
        store.start(lease, &prepared).unwrap();
    }
    fn seed_terminal(
        &self,
        lease: &ProjectMutationLeaseV1,
        archived: bool,
    ) -> (SyncRemoteIntentHistoryV2, ParticipationV2) {
        let first = self.history(1);
        let i = first.current().identity().clone();
        let part = ParticipationV2 {
            schema_version: 2,
            participation: Participation {
                share: ShareLink {
                    remote: i.remote.transport_url(),
                    share_id: i.share_id.as_str().into(),
                    invitation_id: "invite-c10-0001".into(),
                    expires_unix_seconds: 2000,
                },
                manifest: ShareManifest {
                    version: 1,
                    excluded: vec![],
                },
                joined_unix_seconds: 1000,
                agreed_commit: Some(c10_test_oid('b').as_str().into()),
                applied_commit: None,
                unresolved: vec![],
            },
            credential_binding: i.account_binding.clone(),
        };
        let digest = participation::publish_v2(&self.state, lease, None, &part).unwrap();
        let mut receipt = c10_test_receipt(&i);
        receipt.participation_digest = digest;
        let history = first
            .before_send(SyncBeforeSendV2 {
                intended_commit: c10_test_oid('b'),
                reopened_descriptor: i.payload.clone(),
                expected_remote: i.expected_remote_oid.clone(),
            })
            .unwrap()
            .completed(receipt)
            .unwrap();
        let store = self.management(lease);
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = store
            .prepare(lease, store.cursor().unwrap(), self.request(&history))
            .unwrap()
        else {
            panic!("fresh request");
        };
        let started = store.start(lease, &prepared).unwrap();
        store
            .complete_started(
                lease,
                &started,
                hash(history.terminal_evidence_hash().unwrap()),
            )
            .unwrap();
        self.install(&history, archived);
        super::super::owner::C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)
            .expect("terminal fixture must pass the strict C10 reader");
        (history, part)
    }
}
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn write(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn hash(v: SyncDigestV1) -> Hash {
    Hash::from_hex(&v.as_hex()).unwrap()
}

#[test]
fn absent_stores_are_readonly_under_actual_ex_and_sh_without_initialization() {
    let f = Fixture::new();
    let ex = f.owner.try_mutation().unwrap();
    let before = f.inventory();
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.inventory(), before);
    drop(ex);
    let sh = f.owner.try_activity().unwrap();
    assert!(
        C10CompositeBlockerV1::inspect_with_activity(&f.project, &f.state, &sh)
            .unwrap()
            .is_none()
    );
    assert!(ProjectReadLeaseV1::Activity(&sh).mutation().is_err());
    assert_eq!(f.inventory(), before);
    assert!(!f.state.join("sync").exists());
    assert!(!f.state.join("management-effects-v1").exists());
}
#[test]
fn foreign_lease_and_replaced_named_writer_are_refused_without_writes() {
    let f = Fixture::new();
    let other = Fixture::new();
    let foreign = other.owner.try_activity().unwrap();
    let before = f.inventory();
    assert!(C10CompositeBlockerV1::inspect_with_activity(&f.project, &f.state, &foreign).is_err());
    assert_eq!(f.inventory(), before);
    let ex = f.owner.try_mutation().unwrap();
    let lock = f.state.join("rewind-workspace/writer.lease");
    fs::rename(&lock, f.state.join("preserved-writer")).unwrap();
    write(&lock, b"");
    let changed = f.inventory();
    assert!(C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex).is_err());
    assert_eq!(f.inventory(), changed);
}
#[test]
fn orphan_local_prepared_and_missing_terminal_pair_are_refused() {
    for terminal in [false, true] {
        let f = Fixture::new();
        let ex = f.owner.try_mutation().unwrap();
        let h = if terminal {
            let first = f.history(1);
            let i = first.current().identity().clone();
            first
                .before_send(SyncBeforeSendV2 {
                    intended_commit: c10_test_oid('b'),
                    reopened_descriptor: i.payload.clone(),
                    expected_remote: i.expected_remote_oid.clone(),
                })
                .unwrap()
                .completed(c10_test_receipt(&i))
                .unwrap()
        } else {
            f.history(1)
        };
        f.install(&h, terminal);
        let before = f.inventory();
        assert!(C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex).is_err());
        assert_eq!(f.inventory(), before);
    }
}
#[test]
fn matching_started_pair_and_prepared_no_intent_are_blockers_in_both_lease_modes() {
    for with_intent in [false, true] {
        let f = Fixture::new();
        let ex = f.owner.try_mutation().unwrap();
        let h = f.history(1);
        if with_intent {
            f.seed_started(&ex, &h);
            f.install(&h, false);
            super::super::owner::C10IntentStoreV2::inspect_at_state_dir(&f.state, &ex)
                .expect("started fixture must pass the strict C10 reader");
        } else {
            let m = f.management(&ex);
            m.prepare(&ex, m.cursor().unwrap(), f.request(&h)).unwrap();
        }
        let before = f.inventory();
        let effect = C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex)
            .unwrap()
            .unwrap();
        assert_eq!(effect.request.operation_id, h.operation_id());
        assert_eq!(effect.request.inventory.encoded(), "C10");
        assert_eq!(f.inventory(), before);
        drop(ex);
        let sh = f.owner.try_activity().unwrap();
        assert!(
            C10CompositeBlockerV1::inspect_with_activity(&f.project, &f.state, &sh)
                .unwrap()
                .is_some()
        );
        assert_eq!(f.inventory(), before);
    }
}
#[test]
fn active_completed_uses_actual_stored_participation_without_keychain_authority() {
    let f = Fixture::new();
    let ex = f.owner.try_mutation().unwrap();
    let (_, mut part) = f.seed_terminal(&ex, false);
    let before = f.inventory();
    assert!(matches!(
        participation::read_snapshot(&f.state)
            .unwrap()
            .classification,
        participation::ParticipationReadV2::StaleCredentialBindingOnly(_)
    ));
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.inventory(), before);
    let digest = participation::read_snapshot(&f.state).unwrap().exact_digest;
    part.participation.agreed_commit = Some(c10_test_oid('c').as_str().into());
    participation::publish_v2(&f.state, &ex, digest, &part).unwrap();
    let changed = f.inventory();
    assert!(C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex).is_err());
    assert_eq!(f.inventory(), changed);
}
#[test]
fn archived_terminal_survives_management_seal_and_later_participation_generation() {
    let f = Fixture::new();
    let ex = f.owner.try_mutation().unwrap();
    let (h, mut part) = f.seed_terminal(&ex, true);
    let m = f.management(&ex);
    m.test_joined_seal(&ex).unwrap();
    assert!(m.cursor().unwrap().generation() > 0);
    assert!(m.blocker().unwrap().is_none());
    assert!(m.effect(h.operation_id()).unwrap().is_some());
    let old = participation::read_snapshot(&f.state).unwrap().exact_digest;
    part.credential_binding.generation += 1;
    part.credential_binding.credential_item_version = "3".repeat(64);
    part.participation.agreed_commit = Some(c10_test_oid('c').as_str().into());
    participation::publish_v2(&f.state, &ex, old, &part).unwrap();
    let before = f.inventory();
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex)
            .unwrap()
            .is_none()
    );
    drop(ex);
    let sh = f.owner.try_activity().unwrap();
    assert!(
        C10CompositeBlockerV1::inspect_with_activity(&f.project, &f.state, &sh)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.inventory(), before);
}
#[test]
fn corrupt_unknown_legacy_and_pending_entries_fail_closed_without_repair() {
    for mode in 0..4 {
        let f = Fixture::new();
        let ex = f.owner.try_mutation().unwrap();
        let h = f.history(1);
        let path = f.install(&h, false);
        match mode {
            0 => fs::write(&path, b"{torn").unwrap(),
            1 => write(&f.state.join("sync/remote-intents-v2/unknown"), b"retain"),
            2 => {
                directory(&f.state.join("sync/remote-intents-v1"));
                write(&f.state.join("sync/remote-intents-v1/owner.lock"), b"");
                write(&f.state.join("sync/remote-intents-v1/unknown"), b"retain");
            }
            _ => write(
                &f.state.join("sync/remote-intents-v2").join(format!(
                    ".{}.json.1.{}.tmp",
                    SyncDigestV1::digest(h.operation_id().as_hex().as_bytes()).as_hex(),
                    "a".repeat(24)
                )),
                &h.canonical_bytes().unwrap(),
            ),
        }
        let before = f.inventory();
        assert!(C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex).is_err());
        assert_eq!(f.inventory(), before);
    }
}
#[test]
fn fresh_shared_check_detects_writer_between_ex_release_and_activity_acquisition() {
    let f = Fixture::new();
    let ex = f.owner.try_mutation().unwrap();
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex)
            .unwrap()
            .is_none()
    );
    drop(ex);
    // Deterministic gate-to-activity interleaving with the actual same flock.
    let peer = ProjectMutationOwnerV1::open_existing(&f.state.join("rewind-workspace")).unwrap();
    let writer = peer.try_mutation().unwrap();
    let h = f.history(1);
    f.install(&h, false);
    assert!(f.owner.try_activity().is_err());
    drop(writer);
    let sh = f.owner.try_activity().unwrap();
    let before = f.inventory();
    assert!(C10CompositeBlockerV1::inspect_with_activity(&f.project, &f.state, &sh).is_err());
    assert!(peer.try_mutation().is_err());
    assert_eq!(f.inventory(), before);
}
#[test]
fn corrupt_participation_blocks_even_when_sync_and_management_intents_are_absent() {
    let f = Fixture::new();
    let ex = f.owner.try_mutation().unwrap();
    directory(&f.state.join("sync"));
    write(&f.state.join("sync/participation.json"), b"{torn");
    let before = f.inventory();
    assert!(C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex).is_err());
    assert_eq!(f.inventory(), before);
}

#[test]
fn matching_state_and_lease_for_another_project_cannot_hide_the_actual_blocker() {
    let a = Fixture::new();
    let b = Fixture::new();
    a.install(&a.history(1), false);
    let before_a = a.inventory();
    let before_b = b.inventory();
    let ex = b.owner.try_mutation().unwrap();
    assert!(C10CompositeBlockerV1::inspect_with_mutation(&a.project, &b.state, &ex).is_err());
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&b.project, &b.state, &ex)
            .unwrap()
            .is_none()
    );
    drop(ex);
    let sh = b.owner.try_activity().unwrap();
    assert!(C10CompositeBlockerV1::inspect_with_activity(&a.project, &b.state, &sh).is_err());
    assert!(
        C10CompositeBlockerV1::inspect_with_activity(&b.project, &b.state, &sh)
            .unwrap()
            .is_none()
    );
    assert_eq!(a.inventory(), before_a);
    assert_eq!(b.inventory(), before_b);
}

#[test]
fn an_explicit_markerless_workspace_root_does_not_adopt_its_repository_ancestor() {
    let f = Fixture::new();
    fs::remove_dir(f.project.join(".polaris")).unwrap();
    let before = f.inventory();
    let ex = f.owner.try_mutation().unwrap();
    assert!(
        C10CompositeBlockerV1::inspect_with_mutation(&f.project, &f.state, &ex)
            .unwrap()
            .is_none()
    );
    drop(ex);
    let sh = f.owner.try_activity().unwrap();
    assert!(
        C10CompositeBlockerV1::inspect_with_activity(&f.project, &f.state, &sh)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.inventory(), before);
}
