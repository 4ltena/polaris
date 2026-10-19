//! C10 owner store v2 identity, persistence and recovery regression tests.
use super::*;
use crate::management_effect::{
    ManagementEffectScopeV1, ManagementInventoryIdV1, PrepareManagementEffectV1,
};
use crate::project_mutation::ProjectMutationOwnerV1;
use crate::sync::participation::{Participation, ParticipationV2};
use crate::sync::share::{ShareLink, ShareManifest};
use std::{
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    str::FromStr,
};

struct Fixture {
    _temporary: tempfile::TempDir,
    state: PathBuf,
}
impl Fixture {
    fn empty() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _temporary: temporary,
            state: fs::canonicalize(state).unwrap(),
        }
    }
    fn present() -> Self {
        let fixture = Self::empty();
        for path in [
            fixture.state.join("sync"),
            fixture.root(),
            fixture.root().join("archive"),
        ] {
            fixture.directory(&path);
        }
        fixture.write(&fixture.root().join("owner.lock"), &[]);
        fixture
    }
    fn directory(&self, path: &Path) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn root(&self) -> PathBuf {
        self.state.join("sync").join(DIRECTORY)
    }
    fn mutation_owner(&self) -> ProjectMutationOwnerV1 {
        ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace")).unwrap()
    }
    fn write(&self, path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn install(&self, history: &SyncRemoteIntentHistoryV2, archived: bool) -> PathBuf {
        let name = record_name(history.operation_id());
        let directory = if archived {
            let path = self.root().join("archive").join(&name[..2]);
            if !path.exists() {
                self.directory(&path);
            }
            path
        } else {
            self.root()
        };
        let path = directory.join(name);
        self.write(&path, &history.canonical_bytes().unwrap());
        path
    }
    fn temporary(&self, history: &SyncRemoteIntentHistoryV2, suffix: char) -> PathBuf {
        let path = self.root().join(format!(
            ".{}.{}.{}.tmp",
            record_name(history.operation_id()),
            history.current().sequence(),
            suffix.to_string().repeat(24)
        ));
        self.write(&path, &history.canonical_bytes().unwrap());
        path
    }
    fn bytes(&self) -> BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)> {
        fn walk(path: &Path, out: &mut BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)>) {
            let meta = fs::symlink_metadata(path).unwrap();
            let bytes = if meta.is_file() {
                fs::read(path).unwrap()
            } else {
                vec![]
            };
            out.insert(
                path.to_owned(),
                (meta.mode(), meta.ino(), meta.nlink(), bytes),
            );
            if meta.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), out);
                }
            }
        }
        let mut value = BTreeMap::new();
        walk(&self.state, &mut value);
        value
    }
}
fn operation(number: u64) -> Operation {
    Operation::from_str(&format!("{number:032x}")).unwrap()
}
fn next(history: &SyncRemoteIntentHistoryV2) -> SyncRemoteIntentHistoryV2 {
    history
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('b'),
            reopened_descriptor: history.current().identity().payload.clone(),
            expected_remote: history.current().identity().expected_remote_oid.clone(),
        })
        .unwrap()
}
fn part(identity: &SyncIntentIdentityV2, oid: char) -> ParticipationV2 {
    ParticipationV2 {
        schema_version: 2,
        participation: Participation {
            share: ShareLink {
                remote: identity.remote.transport_url(),
                share_id: identity.share_id.as_str().into(),
                invitation_id: "invite-c10-0001".into(),
                expires_unix_seconds: 2000,
            },
            manifest: ShareManifest {
                version: 1,
                excluded: vec![],
            },
            joined_unix_seconds: 1000,
            agreed_commit: Some(c10_test_oid(oid).as_str().into()),
            applied_commit: Some(c10_test_oid('a').as_str().into()),
            unresolved: vec![],
        },
        credential_binding: identity.account_binding.clone(),
    }
}
fn terminal(
    number: u64,
) -> (
    SyncRemoteIntentHistoryV2,
    ParticipationSnapshotV2,
    ManagementEffectSnapshotV1,
) {
    let first = c10_test_history(number, false);
    let identity = first.current().identity();
    let participation = part(identity, 'b');
    let digest = SyncDigestV1::digest(&participation.canonical_bytes().unwrap());
    let mut receipt = c10_test_receipt(identity);
    receipt.participation_digest = digest;
    let history = next(&first).completed(receipt).unwrap();
    let request = PrepareManagementEffectV1 {
        operation_id: identity.operation_id,
        inventory: ManagementInventoryIdV1::parse("C10").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: Hash::from_hex(&identity.project.as_hex()).unwrap(),
            session: None,
            generation: 1,
            revision: 0,
            permission: ManagementPermissionScopeV1::Remote,
            account_binding: Some(identity.management_account_binding_hash().unwrap()),
        },
        request_hash: Hash::from_hex(&identity.request_hash.as_hex()).unwrap(),
        rewind: ManagementRewindDispositionV1::IrreversibleRemote,
    };
    let management = ManagementEffectSnapshotV1 {
        request,
        state: ManagementEffectStateV1::Completed {
            evidence_hash: Hash::from_hex(&history.terminal_evidence_hash().unwrap().as_hex())
                .unwrap(),
        },
        last_sequence: 3,
    };
    (
        history,
        ParticipationSnapshotV2 {
            classification: ParticipationReadV2::Current(participation),
            exact_digest: Some(digest),
        },
        management,
    )
}

#[test]
fn c10_v2_missing_store_inspection_and_reservation_write_zero() {
    let fixture = Fixture::empty();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let digest = store.inventory_digest();
    assert!(
        store
            .pending_history_for_operation(operation(1))
            .unwrap()
            .is_none()
    );
    let _reservation = store.reserve_operation(operation(1)).unwrap();
    assert_eq!(store.inventory_digest(), digest);
    assert_eq!(fixture.bytes(), before);
    assert!(!fixture.root().exists());
}

#[test]
fn c10_v2_publication_roundtrips_entire_immutable_prefix() {
    let fixture = Fixture::empty();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut reservation = store.reserve_operation(operation(1)).unwrap();
    let first = c10_test_history(1, false);
    store.publish(&mut reservation, &first).unwrap();
    assert_eq!(store.lookup(operation(1)).unwrap(), Some(first.clone()));
    let second = next(&first);
    store.publish(&mut reservation, &second).unwrap();
    assert_eq!(store.lookup(operation(1)).unwrap(), Some(second.clone()));
    let before = fixture.bytes();
    store.publish(&mut reservation, &second).unwrap();
    assert_eq!(fixture.bytes(), before);
    assert_eq!(store.active_records().count(), 1);
    assert_eq!(
        fs::metadata(fixture.root().join("owner.lock"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn c10_v2_reservation_rejects_wrong_operation_and_inventory_before_writes() {
    let fixture = Fixture::present();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut reservation = store.reserve_operation(operation(1)).unwrap();
    let before = fixture.bytes();
    assert!(
        store
            .publish(&mut reservation, &c10_test_history(2, false))
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
    fixture.install(&c10_test_history(3, false), false);
    let before = fixture.bytes();
    assert!(
        store
            .publish(&mut reservation, &c10_test_history(1, false))
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_reservation_rejects_another_project_lease() {
    let fixture = Fixture::present();
    let other = Fixture::present();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let other_owner = other.mutation_owner();
    let other_lease = other_owner.try_mutation().unwrap();
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &other_lease).is_err());
    let mut first = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut token = first.reserve_operation(operation(1)).unwrap();
    let mut second = C10IntentStoreV2::inspect_at_state_dir(&other.state, &other_lease).unwrap();
    assert!(
        second
            .publish(&mut token, &c10_test_history(1, false))
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_exact_next_temporary_is_readonly_until_explicit_same_operation_resume() {
    let fixture = Fixture::present();
    let first = c10_test_history(1, false);
    let second = next(&first);
    fixture.install(&first, false);
    let temporary = fixture.temporary(&second, 'a');
    let temporary_inode = fs::metadata(&temporary).unwrap().ino();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    assert!(
        matches!(store.pending(), C10PendingIntentV2::ExactNext { operation_id, sequence: 2, .. } if *operation_id == operation(1))
    );
    assert_eq!(fixture.bytes(), before);
    assert_eq!(
        store.pending_history_for_operation(operation(1)).unwrap(),
        Some(second.clone())
    );
    assert!(store.pending_history_for_operation(operation(2)).is_err());
    assert_eq!(fixture.bytes(), before);
    assert!(store.resume_pending(operation(2)).is_err());
    assert_eq!(fixture.bytes(), before);
    assert_eq!(store.resume_pending(operation(1)).unwrap(), second);
    assert!(!temporary.exists());
    assert_eq!(
        fs::metadata(fixture.root().join(record_name(operation(1))))
            .unwrap()
            .ino(),
        temporary_inode
    );
}

#[test]
fn c10_v2_first_temporary_can_publish_only_sequence_one_zero_previous() {
    let fixture = Fixture::present();
    let first = c10_test_history(1, false);
    fixture.temporary(&first, 'a');
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let before = fixture.bytes();
    assert!(store.lookup(operation(1)).unwrap().is_none());
    assert_eq!(
        store.pending_history_for_operation(operation(1)).unwrap(),
        Some(first.clone())
    );
    assert!(store.pending_history_for_operation(operation(2)).is_err());
    assert_eq!(fixture.bytes(), before);
    assert_eq!(store.resume_pending(operation(1)).unwrap(), first);
    drop(store);
    let other = c10_test_history(2, false);
    fixture.temporary(&next(&other), 'b');
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_conflicting_and_multiple_temporaries_are_preserved() {
    let fixture = Fixture::present();
    let first = c10_test_history(1, false);
    fixture.install(&first, false);
    fixture.temporary(&next(&first), 'a');
    fixture.temporary(&next(&first), 'b');
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    assert_eq!(*store.pending(), C10PendingIntentV2::Conflict);
    assert!(store.pending_history_for_operation(operation(1)).is_err());
    assert!(store.pending_history_for_operation(operation(2)).is_err());
    assert!(store.resume_pending(operation(1)).is_err());
    assert!(store.reserve_operation(operation(1)).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_pending_history_rejects_changed_bytes_inode_and_inventory_without_repair() {
    for mode in 0..3 {
        let fixture = Fixture::present();
        let first = c10_test_history(1, false);
        let temporary = fixture.temporary(&first, 'a');
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        match mode {
            0 => {
                let mut identity = c10_test_identity(1);
                identity.account_binding.git.inode += 1;
                let identity = identity.bind_local_request(&c10_test_prepared()).unwrap();
                let changed =
                    SyncRemoteIntentHistoryV2::local_prepared(identity, c10_test_prepared())
                        .unwrap();
                fs::write(&temporary, changed.canonical_bytes().unwrap()).unwrap();
            }
            1 => {
                fs::rename(&temporary, fixture.state.join("preserved-prior-temporary")).unwrap();
                fixture.write(&temporary, &first.canonical_bytes().unwrap());
            }
            _ => {
                fixture.temporary(&c10_test_history(2, false), 'b');
            }
        }
        let before = fixture.bytes();
        assert!(store.pending_history_for_operation(operation(1)).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_invalid_temp_unknown_name_and_hash_corruption_write_zero() {
    for mode in 0..4 {
        let fixture = Fixture::present();
        let history = c10_test_history(1, false);
        match mode {
            0 => fixture.write(&fixture.root().join("unknown"), b"unchanged"),
            1 => fixture.write(
                &fixture.root().join(format!(
                    ".{}.1.{}.tmp",
                    record_name(operation(1)),
                    "a".repeat(24)
                )),
                b"{torn",
            ),
            2 => {
                let bytes = history.canonical_bytes().unwrap();
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                value["records"][0]["record_hash"] = "0".repeat(64).into();
                fixture.write(
                    &fixture.root().join(record_name(operation(1))),
                    &serde_json::to_vec(&value).unwrap(),
                );
            }
            _ => fixture.write(
                &fixture.root().join(record_name(operation(1))),
                &vec![b' '; MAX_RECORD as usize + 1],
            ),
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let before = fixture.bytes();
        assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_symlink_hardlink_and_wrong_file_mode_are_preserved_blockers() {
    for mode in 0..3 {
        let fixture = Fixture::present();
        let path = fixture.root().join(record_name(operation(1)));
        match mode {
            0 => {
                let outside = fixture.state.join("outside");
                fixture.write(
                    &outside,
                    &c10_test_history(1, false).canonical_bytes().unwrap(),
                );
                symlink(outside, &path).unwrap();
            }
            1 => {
                let outside = fixture.state.join("outside");
                fixture.write(
                    &outside,
                    &c10_test_history(1, false).canonical_bytes().unwrap(),
                );
                fs::hard_link(outside, &path).unwrap();
            }
            _ => {
                fixture.install(&c10_test_history(1, false), false);
                fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
            }
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let before = fixture.bytes();
        assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_existing_root_wrong_mode_is_not_repaired() {
    let fixture = Fixture::present();
    fs::set_permissions(fixture.root(), fs::Permissions::from_mode(0o755)).unwrap();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_owner_lock_requires_exact_empty_private_single_link_and_retains_ex() {
    let fixture = Fixture::present();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    drop(store);
    fs::write(fixture.root().join("owner.lock"), b"x").unwrap();
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_named_directory_and_target_inode_replacement_fail_before_writes() {
    for replace_directory in [false, true] {
        let fixture = Fixture::present();
        let first = c10_test_history(1, false);
        let path = fixture.install(&first, false);
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        let mut reservation = store.reserve_operation(operation(1)).unwrap();
        if replace_directory {
            fs::rename(fixture.root(), fixture.state.join("old-store")).unwrap();
            fixture.directory(&fixture.root());
            fixture.directory(&fixture.root().join("archive"));
            fixture.write(&fixture.root().join("owner.lock"), &[]);
        } else {
            fs::rename(&path, fixture.state.join("old-record")).unwrap();
            fixture.write(&path, &first.canonical_bytes().unwrap());
        }
        let before = fixture.bytes();
        assert!(store.publish(&mut reservation, &next(&first)).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_exact_active_and_physical_root_bounds_do_not_ignore_tail() {
    let fixture = Fixture::present();
    for number in 1..=MAX_ACTIVE as u64 {
        fixture.install(&c10_test_history(number, false), false);
    }
    for number in 1000..1000 + (MAX_TMP - 1) as u64 {
        fixture.temporary(&c10_test_history(number, false), 'a');
    }
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    assert_eq!(store.active_records().count(), MAX_ACTIVE);
    assert_eq!(store.inventory.counts.temporary, MAX_TMP - 1);
    assert_eq!(
        fs::read_dir(fixture.root()).unwrap().count(),
        MAX_ROOT_ENTRIES
    );
    drop(store);
    fixture.temporary(&c10_test_history(2000, false), 'b');
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_full_active_reserves_existing_growth_but_rejects_new_operation_write_zero() {
    let fixture = Fixture::present();
    for number in 1..=MAX_ACTIVE as u64 {
        fixture.install(&c10_test_history(number, false), false);
    }
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let before = fixture.bytes();
    assert!(store.reserve_operation(operation(999)).is_err());
    let _existing = store.reserve_operation(operation(1)).unwrap();
    assert_eq!(fixture.bytes(), before);
    drop(store);
    fixture.install(&c10_test_history(999, false), false);
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_owned_publish_faults_leave_only_classified_evidence_without_cleanup() {
    for point in 5..=10 {
        let fixture = Fixture::present();
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        let mut reservation = store.reserve_operation(operation(1)).unwrap();
        store.test_fail_once_at(point);
        let first = c10_test_history(1, false);
        assert!(
            store.publish(&mut reservation, &first).is_err(),
            "fault {point} was not reached"
        );
        drop(store);
        let before = fixture.bytes();
        match C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease) {
            Err(_) => {
                assert_eq!(point, 6);
                assert_eq!(fixture.bytes(), before);
            }
            Ok(mut reopened) => {
                if matches!(reopened.pending(), C10PendingIntentV2::ExactNext { .. }) {
                    assert!(point == 7 || point == 8);
                    assert_eq!(fixture.bytes(), before);
                    assert_eq!(reopened.resume_pending(operation(1)).unwrap(), first);
                } else if point >= 9 {
                    assert_eq!(reopened.lookup(operation(1)).unwrap(), Some(first));
                } else {
                    assert_eq!(point, 5);
                    assert!(reopened.lookup(operation(1)).unwrap().is_none());
                }
            }
        }
    }
}

#[test]
fn c10_v2_terminal_capability_binds_management_receipt_and_participation() {
    let fixture = Fixture::present();
    let (history, participation, management) = terminal(1);
    fixture.install(&history, false);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let reservation = store.reserve_operation(operation(1)).unwrap();
    let before = fixture.bytes();
    assert!(
        store
            .terminal_readback(&reservation, &management, &participation)
            .is_ok()
    );
    for mode in 0..8 {
        let mut effect = management.clone();
        let mut part = participation.clone();
        match mode {
            0 => effect.request.operation_id = operation(2),
            1 => effect.request.request_hash = Hash::digest(b"wrong request"),
            2 => effect.request.scope.account_binding = None,
            3 => {
                effect.state = ManagementEffectStateV1::Completed {
                    evidence_hash: Hash::digest(b"wrong receipt"),
                }
            }
            4 => part.exact_digest = Some(SyncDigestV1::digest(b"wrong participation")),
            5 => {
                let ParticipationReadV2::Current(value) = &mut part.classification else {
                    unreachable!()
                };
                value.participation.agreed_commit = Some(c10_test_oid('c').as_str().into());
            }
            6 => effect.request.scope.generation += 1,
            _ => {
                let ParticipationReadV2::Current(value) = &mut part.classification else {
                    unreachable!()
                };
                value.credential_binding.credential_item_version = "3".repeat(64);
            }
        }
        assert!(
            store
                .terminal_readback(&reservation, &effect, &part)
                .is_err()
        );
    }
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_archive_preserves_inode_and_direct_lookup_terminal_identity() {
    let fixture = Fixture::present();
    let (history, participation, management) = terminal(1);
    let source = fixture.install(&history, false);
    let inode = fs::metadata(&source).unwrap().ino();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut reservation = store.reserve_operation(operation(1)).unwrap();
    let completion = store
        .terminal_readback(&reservation, &management, &participation)
        .unwrap();
    store
        .archive_terminal(&mut reservation, completion)
        .unwrap();
    assert!(!source.exists());
    let name = record_name(operation(1));
    assert_eq!(
        fs::metadata(fixture.root().join("archive").join(&name[..2]).join(&name))
            .unwrap()
            .ino(),
        inode
    );
    assert_eq!(store.lookup(operation(1)).unwrap(), Some(history.clone()));
    assert!(store.reserve_operation(operation(1)).is_err());
    let before = fixture.bytes();
    drop(store);
    assert_eq!(
        C10IntentStoreV2::direct_lookup_at_state_dir(&fixture.state, &lease, operation(1)).unwrap(),
        Some(history)
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_nonterminal_and_partial_pair_cannot_acquire_archive_capability() {
    let fixture = Fixture::present();
    let first = c10_test_history(1, false);
    fixture.install(&first, false);
    let (_, participation, management) = terminal(1);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let reservation = store.reserve_operation(operation(1)).unwrap();
    let before = fixture.bytes();
    assert!(
        store
            .terminal_readback(&reservation, &management, &participation)
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_archive_faults_preserve_terminal_evidence_and_atomic_location() {
    for point in 11..=14 {
        let fixture = Fixture::present();
        let (history, participation, management) = terminal(1);
        fixture.install(&history, false);
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        let mut reservation = store.reserve_operation(operation(1)).unwrap();
        let completion = store
            .terminal_readback(&reservation, &management, &participation)
            .unwrap();
        store.test_fail_once_at(point);
        assert!(
            store
                .archive_terminal(&mut reservation, completion)
                .is_err(),
            "fault {point} was not reached"
        );
        drop(store);
        let before = fixture.bytes();
        assert_eq!(
            C10IntentStoreV2::direct_lookup_at_state_dir(&fixture.state, &lease, operation(1))
                .unwrap(),
            Some(history)
        );
        assert_eq!(fixture.bytes(), before);
        let name = record_name(operation(1));
        let active = fixture.root().join(&name).exists();
        let archived = fixture
            .root()
            .join("archive")
            .join(&name[..2])
            .join(&name)
            .exists();
        assert_ne!(active, archived);
    }
}

#[test]
fn c10_v2_direct_archive_lookup_does_not_require_successful_blocker_scan() {
    let fixture = Fixture::present();
    let history = c10_test_history(1, true);
    fixture.install(&history, true);
    fixture.write(&fixture.root().join("unknown-active-entry"), b"preserved");
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(
        C10IntentStoreV2::direct_lookup_at_state_dir(&fixture.state, &lease, operation(1)).unwrap(),
        Some(history)
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_duplicate_operation_across_active_archive_and_bad_bucket_fail_closed() {
    let fixture = Fixture::present();
    let history = c10_test_history(1, true);
    fixture.install(&history, true);
    fixture.install(&history, false);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert!(
        C10IntentStoreV2::direct_lookup_at_state_dir(&fixture.state, &lease, operation(1)).is_err()
    );
    assert_eq!(fixture.bytes(), before);
    let other = Fixture::present();
    other.directory(&other.root().join("archive").join("ZZ"));
    let other_owner = other.mutation_owner();
    let other_lease = other_owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&other.state, &other_lease).unwrap();
    let before = other.bytes();
    assert!(store.reserve_operation(operation(1)).is_err());
    assert!(store.archive_page(None).is_err());
    assert_eq!(other.bytes(), before);
}

#[test]
fn c10_v2_archive_pages_are_bounded_and_cursor_is_stable_or_rejected() {
    let fixture = Fixture::present();
    for number in 1..=129 {
        fixture.install(&c10_test_history(number, true), true);
    }
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let first = store.archive_page(None).unwrap();
    assert_eq!(first.records.len(), 128);
    let cursor = first.next.unwrap();
    let second = store.archive_page(Some(&cursor)).unwrap();
    assert_eq!(second.records.len(), 1);
    assert!(second.next.is_none());
    let ids: BTreeSet<_> = first
        .records
        .iter()
        .chain(&second.records)
        .map(|history| history.operation_id())
        .collect();
    assert_eq!(ids.len(), 129);
    fixture.install(&c10_test_history(200, true), true);
    let before = fixture.bytes();
    assert!(store.archive_page(Some(&cursor)).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_archive_4096_records_retained_and_plus_one_blocks_without_gc() {
    let fixture = Fixture::present();
    for number in 1..=MAX_ARCHIVE as u64 {
        fixture.install(&c10_test_history(number, true), true);
    }
    let (active, participation, management) = terminal(5000);
    fixture.install(&active, false);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    assert_eq!(store.scan_archive().unwrap().count, MAX_ARCHIVE);
    let before = fixture.bytes();
    assert!(store.reserve_operation(operation(9999)).is_err());
    assert!(store.reserve_operation(operation(5000)).is_err());
    assert_eq!(
        store
            .active_terminal_pair_readback(operation(5000), &management, &participation)
            .unwrap(),
        active
    );
    assert_eq!(fixture.bytes(), before);
    drop(store);
    fixture.install(&c10_test_history(9999, true), true);
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::archive_page_at_state_dir(&fixture.state, &lease, None).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_no_intent_inventory_evidence_is_exact_and_terminal() {
    let fixture = Fixture::present();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut reservation = store.reserve_operation(operation(1)).unwrap();
    let wrong = SyncRemoteIntentHistoryV2::no_intent_abandon(c10_test_no_intent(
        1,
        SyncDigestV1::digest(b"wrong inventory"),
    ))
    .unwrap();
    let before = fixture.bytes();
    assert!(store.publish(&mut reservation, &wrong).is_err());
    assert_eq!(fixture.bytes(), before);
    let history = SyncRemoteIntentHistoryV2::no_intent_abandon(c10_test_no_intent(
        1,
        store.inventory_digest(),
    ))
    .unwrap();
    store.publish(&mut reservation, &history).unwrap();
    assert!(store.lookup(operation(1)).unwrap().unwrap().is_terminal());
}

#[test]
fn c10_v1_diagnosis_is_bounded_redacted_and_readonly() {
    let fixture = Fixture::present();
    let legacy = fixture.state.join("sync").join("remote-intents-v1");
    fixture.directory(&legacy);
    fixture.write(&legacy.join("owner.lock"), &[]);
    let request = SyncRemoteIntentRequestV1 {
        operation_id: SyncRemoteOperationIdV1::parse("legacy-operation").unwrap(),
        idempotency_key: SyncRemoteIdempotencyKeyV1::parse("legacy-idempotency").unwrap(),
        project: SyncDigestV1::digest(b"project"),
        share_id: SyncShareIdV1::parse("legacy-share").unwrap(),
        canonical_remote: CanonicalSyncRemoteV1::from_canonical(
            "https://hidden.invalid/team/private.git",
        )
        .unwrap(),
        remote_ref: SyncRemoteRefV1::parse("refs/heads/polaris").unwrap(),
        local_commit: c10_test_oid('b'),
        expected_remote_oid: ExpectedSyncRemoteOidV1::Missing,
        account_binding: SyncAccountBindingV1::new(
            SyncAccountProviderV1::parse("legacy-provider").unwrap(),
            SyncAccountRegistrationIdV1::parse("legacy-registration").unwrap(),
            SyncDigestV1::digest(b"subject"),
        ),
        permission_hash: SyncDigestV1::digest(b"permission"),
    };
    let record = SyncRemoteIntentRecordV1::before_send(request);
    let name = format!(
        "{}.json",
        SyncDigestV1::digest(record.request.operation_id.as_str().as_bytes()).as_hex()
    );
    fixture.write(
        &legacy.join(&name),
        &serde_json::to_vec_pretty(&record).unwrap(),
    );
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let before = fixture.bytes();
    let diagnosis = store.legacy_diagnostics().unwrap();
    assert_eq!(diagnosis.len(), 1);
    assert!(diagnosis[0].manual_action_required);
    let printed = format!("{diagnosis:?}");
    assert!(!printed.contains("hidden.invalid"));
    assert!(!printed.contains("private.git"));
    assert_eq!(fixture.bytes(), before);
    fs::write(legacy.join(name), vec![b' '; MAX_RECORD as usize + 1]).unwrap();
    let before = fixture.bytes();
    assert!(store.legacy_diagnostics().is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_valid_but_rewritten_temporary_prefix_is_preserved_corruption() {
    let fixture = Fixture::present();
    let first = c10_test_history(1, false);
    fixture.install(&first, false);
    let mut changed = c10_test_identity(1);
    changed.account_binding.git.inode += 1;
    let changed = changed.bind_local_request(&c10_test_prepared()).unwrap();
    let rewrite = SyncRemoteIntentHistoryV2::local_prepared(changed, c10_test_prepared()).unwrap();
    fixture.temporary(&next(&rewrite), 'a');
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_stale_terminal_capability_cannot_move_any_record() {
    let fixture = Fixture::present();
    let (history, participation, management) = terminal(1);
    fixture.install(&history, false);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut reservation = store.reserve_operation(operation(1)).unwrap();
    let completion = store
        .terminal_readback(&reservation, &management, &participation)
        .unwrap();
    fixture.install(&c10_test_history(2, false), false);
    let before = fixture.bytes();
    assert!(
        store
            .archive_terminal(&mut reservation, completion)
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_no_intent_abandon_archive_requires_exact_reconciled_pair() {
    let fixture = Fixture::present();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let mut reservation = store.reserve_operation(operation(1)).unwrap();
    let prior = part(&c10_test_identity(1), 'a');
    let prior_digest = SyncDigestV1::digest(&prior.canonical_bytes().unwrap());
    let mut evidence = c10_test_no_intent(1, store.inventory_digest());
    evidence.identity.prior_participation_digest = prior_digest;
    evidence.identity = evidence
        .identity
        .bind_local_request(&c10_test_prepared())
        .unwrap();
    let history = SyncRemoteIntentHistoryV2::no_intent_abandon(evidence).unwrap();
    store.publish(&mut reservation, &history).unwrap();
    let (_, _, mut management) = terminal(1);
    management.request.request_hash =
        Hash::from_hex(&history.current().identity().request_hash.as_hex()).unwrap();
    management.state = ManagementEffectStateV1::ReconciledAbandon {
        evidence_hash: Hash::from_hex(&history.terminal_evidence_hash().unwrap().as_hex()).unwrap(),
    };
    let participation = ParticipationSnapshotV2 {
        classification: ParticipationReadV2::Current(prior),
        exact_digest: Some(prior_digest),
    };
    let before = fixture.bytes();
    assert_eq!(
        store
            .active_terminal_pair_readback(operation(1), &management, &participation)
            .unwrap(),
        history
    );
    assert_eq!(fixture.bytes(), before);
    let completion = store
        .terminal_readback(&reservation, &management, &participation)
        .unwrap();
    store
        .archive_terminal(&mut reservation, completion)
        .unwrap();
    assert_eq!(store.lookup(operation(1)).unwrap(), Some(history.clone()));
    let before = fixture.bytes();
    assert_eq!(
        store
            .archived_terminal_pair_readback(operation(1), &management)
            .unwrap(),
        history
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_archived_d1_remains_valid_after_current_d2_and_same_share_rebind() {
    let fixture = Fixture::present();
    let (old, old_participation, old_management) = terminal(1);
    fixture.install(&old, true);
    let mut prepared = c10_test_prepared();
    prepared.prior_head = SyncPriorHeadV2::Present {
        commit: c10_test_oid('b'),
    };
    prepared.agreed_commit = Some(c10_test_oid('b'));
    prepared.expected_remote = ExpectedSyncRemoteOidV1::Present {
        oid: c10_test_oid('b'),
    };
    prepared.head_tree = prepared.payload;
    prepared.payload = SyncPayloadDescriptorV1::new(vec![SyncPayloadEntryV1 {
        path: "src/main.rs".into(),
        mode: SyncPayloadModeV1::Regular,
        bytes: 3,
        content_hash: SyncDigestV1::digest(b"def"),
    }])
    .unwrap();
    let mut identity = c10_test_identity(2);
    identity.payload = prepared.payload.clone();
    identity.expected_remote_oid = prepared.expected_remote.clone();
    identity.prior_participation_digest = old_participation.exact_digest.unwrap();
    let identity = identity.bind_local_request(&prepared).unwrap();
    let current = part(&identity, 'c');
    let current_digest = SyncDigestV1::digest(&current.canonical_bytes().unwrap());
    let mut receipt = c10_test_receipt(&identity);
    receipt.intended_commit = c10_test_oid('c');
    receipt.observed_oid = c10_test_oid('c');
    receipt.participation_digest = current_digest;
    let new = SyncRemoteIntentHistoryV2::local_prepared(identity.clone(), prepared)
        .unwrap()
        .before_send(SyncBeforeSendV2 {
            intended_commit: c10_test_oid('c'),
            reopened_descriptor: identity.payload.clone(),
            expected_remote: identity.expected_remote_oid.clone(),
        })
        .unwrap()
        .completed(receipt)
        .unwrap();
    let (_, _, mut new_management) = terminal(2);
    new_management.request.request_hash = Hash::from_hex(&identity.request_hash.as_hex()).unwrap();
    new_management.state = ManagementEffectStateV1::Completed {
        evidence_hash: Hash::from_hex(&new.terminal_evidence_hash().unwrap().as_hex()).unwrap(),
    };
    fixture.install(&new, false);
    let participation_path = fixture
        .state
        .join("sync")
        .join(crate::sync::participation::FILE);
    fixture.write(&participation_path, &current.canonical_bytes().unwrap());
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let reservation = store.reserve_operation(operation(2)).unwrap();
    let snapshot = ParticipationSnapshotV2 {
        classification: ParticipationReadV2::Current(current.clone()),
        exact_digest: Some(current_digest),
    };
    assert_ne!(snapshot.exact_digest, old_participation.exact_digest);
    assert!(
        store
            .terminal_readback(&reservation, &new_management, &snapshot)
            .is_ok()
    );
    let before = fixture.bytes();
    assert_eq!(
        store
            .active_terminal_pair_readback(operation(2), &new_management, &snapshot)
            .unwrap(),
        new
    );
    assert_eq!(
        store
            .archived_terminal_pair_readback(operation(1), &old_management)
            .unwrap(),
        old
    );
    assert!(
        store
            .terminal_readback(&reservation, &new_management, &old_participation)
            .is_err()
    );
    assert!(
        store
            .active_terminal_pair_readback(operation(2), &new_management, &old_participation)
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
    let mut rebound = current;
    rebound.credential_binding.credential_item_version = "3".repeat(64);
    rebound.credential_binding.generation += 1;
    let rebound_bytes = rebound.canonical_bytes().unwrap();
    fs::write(&participation_path, &rebound_bytes).unwrap();
    let rebound_snapshot = ParticipationSnapshotV2 {
        classification: ParticipationReadV2::Current(rebound),
        exact_digest: Some(SyncDigestV1::digest(&rebound_bytes)),
    };
    let before = fixture.bytes();
    assert!(
        store
            .terminal_readback(&reservation, &new_management, &rebound_snapshot)
            .is_err()
    );
    assert!(
        store
            .active_terminal_pair_readback(operation(2), &new_management, &rebound_snapshot)
            .is_err()
    );
    assert_eq!(
        store
            .archived_terminal_pair_readback(operation(1), &old_management)
            .unwrap(),
        old
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_archived_pair_rejects_management_request_scope_state_and_evidence_write_zero() {
    let fixture = Fixture::present();
    let (history, _, management) = terminal(1);
    fixture.install(&history, true);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let before = fixture.bytes();
    for mode in 0..12 {
        let mut changed = management.clone();
        match mode {
            0 => changed.request.operation_id = operation(2),
            1 => changed.request.inventory = ManagementInventoryIdV1::parse("C08").unwrap(),
            2 => changed.request.request_hash = Hash::digest(b"wrong request"),
            3 => changed.request.scope.project = Hash::digest(b"wrong project"),
            4 => changed.request.scope.session = Some(Hash::digest(b"session")),
            5 => changed.request.scope.generation += 1,
            6 => changed.request.scope.permission = ManagementPermissionScopeV1::Project,
            7 => changed.request.scope.account_binding = None,
            8 => changed.request.rewind = ManagementRewindDispositionV1::CapturedRestorable,
            9 => changed.state = ManagementEffectStateV1::Prepared,
            10 => {
                changed.state = ManagementEffectStateV1::Completed {
                    evidence_hash: Hash::digest(b"wrong terminal evidence"),
                }
            }
            _ => {
                changed.state = ManagementEffectStateV1::ReconciledAbandon {
                    evidence_hash: Hash::from_hex(
                        &history.terminal_evidence_hash().unwrap().as_hex(),
                    )
                    .unwrap(),
                }
            }
        }
        assert!(
            store
                .archived_terminal_pair_readback(operation(1), &changed)
                .is_err(),
            "mismatch {mode}"
        );
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_archived_pair_requires_actual_archive_terminal_without_active_duplicate() {
    for mode in 0..4 {
        let fixture = Fixture::present();
        let (history, _, management) = terminal(1);
        match mode {
            0 => {}
            1 => {
                fixture.install(&history, false);
            }
            2 => {
                fixture.install(&c10_test_history(1, false), true);
            }
            _ => {
                fixture.install(&history, false);
                fixture.install(&history, true);
            }
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let store = C10IntentStoreV2::pin(&fixture.state, &lease).unwrap();
        let before = fixture.bytes();
        assert!(
            store
                .archived_terminal_pair_readback(operation(1), &management)
                .is_err(),
            "archive location {mode}"
        );
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_archived_pair_rejects_corrupt_record_and_filename_operation_mismatch() {
    for mismatch in [false, true] {
        let fixture = Fixture::present();
        let (_, _, management) = terminal(1);
        let name = record_name(operation(1));
        let bucket = fixture.root().join("archive").join(&name[..2]);
        fixture.directory(&bucket);
        let bytes = if mismatch {
            terminal(2).0.canonical_bytes().unwrap()
        } else {
            b"{corrupt}".to_vec()
        };
        fixture.write(&bucket.join(name), &bytes);
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let store = C10IntentStoreV2::pin(&fixture.state, &lease).unwrap();
        let before = fixture.bytes();
        assert!(
            store
                .archived_terminal_pair_readback(operation(1), &management)
                .is_err()
        );
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_active_pair_reads_without_reservation_and_keeps_full_current_validation() {
    let fixture = Fixture::present();
    let (history, participation, management) = terminal(1);
    fixture.install(&history, false);
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let before = fixture.bytes();
    assert_eq!(
        store
            .active_terminal_pair_readback(operation(1), &management, &participation)
            .unwrap(),
        history
    );
    for mode in 0..8 {
        let mut changed = participation.clone();
        match mode {
            0 => changed.exact_digest = None,
            1 => changed.exact_digest = Some(SyncDigestV1::digest(b"wrong current bytes")),
            2..=5 => {
                let ParticipationReadV2::Current(part) = &mut changed.classification else {
                    unreachable!()
                };
                match mode {
                    2 => {
                        part.credential_binding.credential_item_version = "3".repeat(64);
                        part.credential_binding.generation += 1;
                    }
                    3 => part.participation.share.share_id = "other-share".into(),
                    4 => {
                        part.participation.share.remote =
                            "https://other.invalid:443/team/project.git".into();
                        part.credential_binding.remote =
                            polaris_auth::git_credentials::CanonicalHttpsRemoteV1::parse(
                                &part.participation.share.remote,
                            )
                            .unwrap();
                    }
                    _ => part.participation.agreed_commit = Some(c10_test_oid('c').as_str().into()),
                }
            }
            6 => changed.classification = ParticipationReadV2::Missing,
            _ => {
                let ParticipationReadV2::Current(part) = changed.classification else {
                    unreachable!()
                };
                changed.classification = ParticipationReadV2::StaleCredentialBindingOnly(part);
            }
        }
        assert!(
            store
                .active_terminal_pair_readback(operation(1), &management, &changed)
                .is_err(),
            "current mismatch {mode}"
        );
        assert_eq!(fixture.bytes(), before);
    }
    let mut wrong_management = management;
    wrong_management.request.request_hash = Hash::digest(b"another request");
    assert!(
        store
            .active_terminal_pair_readback(operation(1), &wrong_management, &participation)
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_active_pair_requires_actual_active_terminal_without_archive_duplicate() {
    for mode in 0..4 {
        let fixture = Fixture::present();
        let (history, participation, management) = terminal(1);
        match mode {
            0 => {}
            1 => {
                fixture.install(&history, true);
            }
            2 => {
                fixture.install(&history, false);
                fixture.install(&history, true);
            }
            _ => {
                fixture.install(&c10_test_history(1, false), false);
            }
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let store = C10IntentStoreV2::pin(&fixture.state, &lease).unwrap();
        let before = fixture.bytes();
        assert!(
            store
                .active_terminal_pair_readback(operation(1), &management, &participation)
                .is_err(),
            "active location {mode}"
        );
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_active_pair_rejects_corrupt_noncanonical_and_filename_operation_mismatch() {
    for mode in 0..3 {
        let fixture = Fixture::present();
        let (history, participation, management) = terminal(1);
        let bytes = match mode {
            0 => b"{corrupt}".to_vec(),
            1 => terminal(2).0.canonical_bytes().unwrap(),
            _ => {
                let mut bytes = history.canonical_bytes().unwrap();
                bytes.push(b'\n');
                bytes
            }
        };
        fixture.write(&fixture.root().join(record_name(operation(1))), &bytes);
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let store = C10IntentStoreV2::pin(&fixture.state, &lease).unwrap();
        let before = fixture.bytes();
        assert!(
            store
                .active_terminal_pair_readback(operation(1), &management, &participation)
                .is_err()
        );
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_255_active_and_32_temporaries_fit_literal_289_entry_root() {
    let fixture = Fixture::present();
    for number in 1..MAX_ACTIVE as u64 {
        fixture.install(&c10_test_history(number, false), false);
    }
    for number in 1000..1000 + MAX_TMP as u64 {
        fixture.temporary(&c10_test_history(number, false), 'a');
    }
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let before = fixture.bytes();
    let store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    assert_eq!(store.inventory.counts.active, MAX_ACTIVE - 1);
    assert_eq!(store.inventory.counts.temporary, MAX_TMP);
    assert_eq!(fs::read_dir(fixture.root()).unwrap().count(), 289);
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn c10_v2_empty_initial_layout_is_readonly_and_explicitly_completed_in_place() {
    for layout in [
        C10InitialLayoutV2::Missing,
        C10InitialLayoutV2::EmptyRoot,
        C10InitialLayoutV2::ArchiveOnly,
        C10InitialLayoutV2::LockOnly,
    ] {
        let fixture = Fixture::empty();
        if layout != C10InitialLayoutV2::Missing {
            fixture.directory(&fixture.state.join("sync"));
            fixture.directory(&fixture.root());
        }
        if layout == C10InitialLayoutV2::ArchiveOnly {
            fixture.directory(&fixture.root().join("archive"));
        }
        if layout == C10InitialLayoutV2::LockOnly {
            fixture.write(&fixture.root().join("owner.lock"), &[]);
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let before = fixture.bytes();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        assert_eq!(store.initial_layout(), layout);
        assert_eq!(store.active_records().count(), 0);
        assert_eq!(store.pending(), &C10PendingIntentV2::None);
        let mut reservation = store.reserve_operation(operation(1)).unwrap();
        assert_eq!(fixture.bytes(), before);
        store.complete_initial_layout(&mut reservation).unwrap();
        assert_eq!(store.initial_layout(), C10InitialLayoutV2::Complete);
        let completed = fixture.bytes();
        for path in [
            fixture.root(),
            fixture.root().join("archive"),
            fixture.root().join("owner.lock"),
        ] {
            if let Some(previous) = before.get(&path) {
                let current = &completed[&path];
                assert_eq!((current.0, current.1), (previous.0, previous.1));
                if path.ends_with("owner.lock") {
                    assert_eq!(current, previous);
                }
            }
        }
        let history = SyncRemoteIntentHistoryV2::no_intent_abandon(c10_test_no_intent(
            1,
            store.inventory_digest(),
        ))
        .unwrap();
        store.publish(&mut reservation, &history).unwrap();
        assert_eq!(store.lookup(operation(1)).unwrap(), Some(history));
    }
}

#[test]
fn c10_v2_initialization_faults_3_and_4_restart_readonly_then_allow_no_intent_abandon() {
    for point in [3, 4] {
        let fixture = Fixture::empty();
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        let mut reservation = store.reserve_operation(operation(1)).unwrap();
        store.test_fail_once_at(point);
        assert!(
            store
                .publish(&mut reservation, &c10_test_history(1, false))
                .is_err()
        );
        drop(store);
        let before = fixture.bytes();
        let mut reopened = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        assert_eq!(
            reopened.initial_layout(),
            if point == 3 {
                C10InitialLayoutV2::EmptyRoot
            } else {
                C10InitialLayoutV2::ArchiveOnly
            }
        );
        assert!(reopened.lookup(operation(1)).unwrap().is_none());
        assert_eq!(reopened.active_records().count(), 0);
        let mut reservation = reopened.reserve_operation(operation(1)).unwrap();
        let inventory_digest = reopened.inventory_digest();
        assert_eq!(fixture.bytes(), before);
        let history =
            SyncRemoteIntentHistoryV2::no_intent_abandon(c10_test_no_intent(1, inventory_digest))
                .unwrap();
        reopened.publish(&mut reservation, &history).unwrap();
        assert_eq!(reopened.lookup(operation(1)).unwrap(), Some(history));
        let after = fixture.bytes();
        assert_eq!(after[&fixture.root()].1, before[&fixture.root()].1);
        if point == 4 {
            assert_eq!(
                after[&fixture.root().join("archive")].1,
                before[&fixture.root().join("archive")].1
            );
        }
    }
}

#[test]
fn c10_v2_incomplete_controls_never_admit_records_unknown_entries_or_nonempty_archive() {
    for mode in 0..6 {
        let fixture = Fixture::empty();
        fixture.directory(&fixture.state.join("sync"));
        fixture.directory(&fixture.root());
        match mode {
            0 => fixture.write(&fixture.root().join("unknown"), b"preserved"),
            1 => {
                fixture.install(&c10_test_history(1, false), false);
            }
            2 => {
                fixture.temporary(&c10_test_history(1, false), 'a');
            }
            3 => {
                fixture.directory(&fixture.root().join("archive"));
                fixture.directory(&fixture.root().join("archive").join("aa"));
            }
            4 => {
                fixture.directory(&fixture.root().join("archive"));
                fixture.write(
                    &fixture.root().join("archive").join("unknown"),
                    b"preserved",
                );
            }
            _ => fixture.write(&fixture.root().join("owner.lock"), b"nonempty"),
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let before = fixture.bytes();
        assert!(C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_initial_directory_inventory_change_rejects_publication_before_any_write() {
    for mode in 0..3 {
        let fixture = Fixture::empty();
        fixture.directory(&fixture.state.join("sync"));
        if mode != 0 {
            fixture.directory(&fixture.root());
        }
        if mode == 2 {
            fixture.directory(&fixture.root().join("archive"));
        }
        let owner = fixture.mutation_owner();
        let lease = owner.try_mutation().unwrap();
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
        let mut reservation = store.reserve_operation(operation(1)).unwrap();
        match mode {
            0 => fixture.directory(&fixture.root()),
            1 => fixture.directory(&fixture.root().join("archive")),
            _ => fixture.directory(&fixture.root().join("archive").join("aa")),
        }
        let before = fixture.bytes();
        assert!(
            store
                .publish(&mut reservation, &c10_test_history(1, false))
                .is_err()
        );
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn c10_v2_reservation_moves_with_owned_lease_and_rejects_same_project_reacquisition() {
    struct OwnedPreparation {
        lease: ProjectMutationLeaseV1,
        reservation: C10IntentReservationV2,
    }
    let fixture = Fixture::present();
    let owner = fixture.mutation_owner();
    let lease = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &lease).unwrap();
    let reservation = store.reserve_operation(operation(1)).unwrap();
    drop(store);
    let preparation = OwnedPreparation { lease, reservation };
    let mut moved = Box::new(preparation);
    let first = c10_test_history(1, false);
    {
        let preparation = moved.as_mut();
        let mut store =
            C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &preparation.lease).unwrap();
        store.publish(&mut preparation.reservation, &first).unwrap();
        assert_eq!(store.lookup(operation(1)).unwrap(), Some(first.clone()));
    }
    let OwnedPreparation {
        lease,
        mut reservation,
    } = *moved;
    drop(lease);
    let reacquired = owner.try_mutation().unwrap();
    let mut store = C10IntentStoreV2::inspect_at_state_dir(&fixture.state, &reacquired).unwrap();
    let before = fixture.bytes();
    assert!(store.publish(&mut reservation, &next(&first)).is_err());
    assert_eq!(fixture.bytes(), before);
}
