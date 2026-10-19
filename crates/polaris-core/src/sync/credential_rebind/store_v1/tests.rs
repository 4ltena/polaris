//! Credential rebind store ownership, archive and persistence regression tests.
use super::super::{test_extend_stored_record, test_no_intent_record, test_stored_record};
use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

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
        let state = fs::canonicalize(state).unwrap();
        Self {
            _temporary: temporary,
            state,
        }
    }

    fn present() -> Self {
        let fixture = Self::empty();
        for directory in [
            fixture.state.join("sync"),
            fixture.root(),
            fixture.archive(),
        ] {
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fixture.write(&fixture.root().join("owner.lock"), &[]);
        fixture
    }

    fn root(&self) -> PathBuf {
        self.state.join("sync").join(DIRECTORY)
    }

    fn archive(&self) -> PathBuf {
        self.root().join("archive")
    }

    fn write(&self, path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn install(&self, record: C08StoredRecordV1, archived: bool) {
        let directory = if archived {
            self.archive()
        } else {
            self.root()
        };
        self.write(
            &directory.join(record_name(&record)),
            &record.canonical_bytes().unwrap(),
        );
    }

    fn bytes(&self) -> BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)> {
        fn walk(directory: &Path, out: &mut BTreeMap<PathBuf, (u32, u64, u64, Vec<u8>)>) {
            let metadata = fs::symlink_metadata(directory).unwrap();
            let bytes = if metadata.is_file() {
                fs::read(directory).unwrap()
            } else {
                Vec::new()
            };
            out.insert(
                directory.to_owned(),
                (metadata.mode(), metadata.ino(), metadata.nlink(), bytes),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(directory).unwrap() {
                    walk(&entry.unwrap().path(), out);
                }
            }
        }
        let mut result = BTreeMap::new();
        walk(&self.state, &mut result);
        result
    }
}

fn id(number: u64) -> ManagementOperationIdV1 {
    ManagementOperationIdV1::from_str(&format!("{number:032x}")).unwrap()
}

#[test]
fn missing_store_inspection_and_reservation_do_not_create_entries() {
    let fixture = Fixture::empty();
    let before = fixture.bytes();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    let initial_digest = store.inventory_digest();
    assert!(store.operation_absent(id(1)));
    assert_eq!(store.records().count(), 0);
    store.reserve_operation(id(1)).unwrap();
    store.refresh().unwrap();
    assert_eq!(store.inventory_digest(), initial_digest);
    assert_eq!(fixture.bytes(), before);
    assert!(!fixture.root().exists());
}

#[test]
fn unreserved_publication_has_zero_filesystem_effect() {
    let fixture = Fixture::empty();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    let before = fixture.bytes();
    assert!(store.publish(&test_stored_record(id(1), false)).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn history_extension_terminal_archive_and_restart_preserve_exact_bytes() {
    let fixture = Fixture::empty();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    let record = test_stored_record(id(1), false);
    let first_bytes = record.canonical_bytes().unwrap();
    store.reserve_operation(id(1)).unwrap();
    store.publish(&record).unwrap();
    assert_eq!(
        fs::read(fixture.root().join(format!("{}.json", id(1)))).unwrap(),
        first_bytes
    );
    let next = test_extend_stored_record(store.lookup(id(1)).unwrap(), true);
    let next_bytes = next.canonical_bytes().unwrap();
    store.publish(&next).unwrap();
    assert!(!store.operation_absent(id(1)));
    store.archive_terminal(id(1)).unwrap();
    assert!(!fixture.root().join(format!("{}.json", id(1))).exists());
    assert_eq!(
        fs::read(fixture.archive().join(format!("{}.json", id(1)))).unwrap(),
        next_bytes
    );
    let digest = store.inventory_digest();
    drop(store);
    let mut restarted = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    assert_eq!(restarted.inventory_digest(), digest);
    assert_eq!(
        restarted.lookup(id(1)).unwrap().canonical_bytes().unwrap(),
        next_bytes
    );
    assert_eq!(restarted.records().count(), 1);
    let before = fixture.bytes();
    restarted.archive_terminal(id(1)).unwrap();
    assert!(restarted.reserve_operation(id(1)).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn no_intent_evidence_has_its_closed_filename_and_is_immutable() {
    let fixture = Fixture::present();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    store.reserve_operation(id(2)).unwrap();
    let record = test_no_intent_record(id(2));
    let bytes = record.canonical_bytes().unwrap();
    store.publish(&record).unwrap();
    let name = format!("{}.no-intent.json", id(2));
    assert_eq!(fs::read(fixture.root().join(&name)).unwrap(), bytes);
    store.publish(&test_no_intent_record(id(2))).unwrap();
    let before = fixture.bytes();
    assert!(store.publish(&test_stored_record(id(2), false)).is_err());
    assert_eq!(fixture.bytes(), before);
    store.archive_terminal(id(2)).unwrap();
    assert_eq!(fs::read(fixture.archive().join(name)).unwrap(), bytes);
}

#[test]
fn readonly_owner_lock_conflict_does_not_change_inventory() {
    let fixture = Fixture::present();
    let first = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    let before = fixture.bytes();
    assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
    assert_eq!(fixture.bytes(), before);
    drop(first);
    assert!(C08IntentStoreV1::inspect(&fixture.state).is_ok());
}

#[test]
fn named_directory_replacement_is_detected_with_original_descriptors_held() {
    for relative in [
        "sync",
        "sync/credential-rebind-v1",
        "sync/credential-rebind-v1/archive",
    ] {
        let fixture = Fixture::present();
        let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
        let original = fixture.state.join(relative);
        let displaced = fixture.state.join("displaced");
        fs::rename(&original, &displaced).unwrap();
        fs::create_dir(&original).unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(0o700)).unwrap();
        let before = fixture.bytes();
        assert!(store.refresh().is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn named_state_replacement_is_detected_without_writing_displaced_tree() {
    let fixture = Fixture::present();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    let old_state = fixture.state.with_file_name("old-state");
    fs::rename(&fixture.state, &old_state).unwrap();
    fs::create_dir(&fixture.state).unwrap();
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(store.refresh().is_err());
    assert!(fs::read_dir(&fixture.state).unwrap().next().is_none());
    assert!(
        old_state
            .join("sync")
            .join(DIRECTORY)
            .join("owner.lock")
            .exists()
    );
}

#[test]
fn existing_record_change_is_not_adopted_as_a_fresh_snapshot() {
    let fixture = Fixture::present();
    fixture.install(test_stored_record(id(1), false), false);
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    let path = fixture.root().join(format!("{}.json", id(1)));
    let changed = test_stored_record(id(1), true).canonical_bytes().unwrap();
    fs::write(&path, &changed).unwrap();
    let before = fixture.bytes();
    assert!(store.refresh().is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn unknown_symlink_hardlink_and_wrong_mode_entries_are_not_repaired() {
    for case in 0..5 {
        let fixture = Fixture::present();
        let record = test_stored_record(id(1), true);
        let name = record_name(&record);
        match case {
            0 => fixture.write(&fixture.root().join("surprise"), b"unknown"),
            1 => {
                let outside = fixture.state.join("outside");
                fixture.write(&outside, &record.canonical_bytes().unwrap());
                symlink(&outside, fixture.root().join(&name)).unwrap();
            }
            2 => {
                let outside = fixture.state.join("outside");
                fixture.write(&outside, &record.canonical_bytes().unwrap());
                fs::hard_link(&outside, fixture.root().join(&name)).unwrap();
            }
            3 => {
                fixture.install(record, false);
                fs::set_permissions(
                    fixture.root().join(&name),
                    fs::Permissions::from_mode(0o640),
                )
                .unwrap();
            }
            4 => fs::set_permissions(fixture.archive(), fs::Permissions::from_mode(0o750)).unwrap(),
            _ => unreachable!(),
        }
        let before = fixture.bytes();
        assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn existing_incomplete_store_does_not_recreate_lock_or_archive() {
    for missing_lock in [false, true] {
        let fixture = Fixture::present();
        if missing_lock {
            fs::remove_file(fixture.root().join("owner.lock")).unwrap();
        } else {
            fs::remove_dir(fixture.archive()).unwrap();
        }
        let before = fixture.bytes();
        assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn noncanonical_corrupt_and_misnamed_records_are_rejected() {
    for case in 0..5 {
        let fixture = Fixture::present();
        let record = test_stored_record(id(1), true);
        let mut bytes = record.canonical_bytes().unwrap();
        let mut name = record_name(&record);
        match case {
            0 => bytes.push(b'\n'),
            1 => {
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("unknown".to_owned(), serde_json::json!(true));
                bytes = serde_json::to_vec(&value).unwrap();
            }
            2 => name = format!("{}.json", id(2)),
            3 => name = format!("{}.no-intent.json", id(1)),
            4 => bytes = b"not JSON".to_vec(),
            _ => unreachable!(),
        }
        fixture.write(&fixture.root().join(name), &bytes);
        let before = fixture.bytes();
        assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn duplicate_operation_across_active_and_archive_is_a_blocker() {
    let fixture = Fixture::present();
    fixture.install(test_stored_record(id(1), true), false);
    fixture.install(test_stored_record(id(1), true), true);
    let before = fixture.bytes();
    assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn nonterminal_archive_and_terminal_replacement_preserve_records() {
    let fixture = Fixture::present();
    fixture.install(test_stored_record(id(1), false), false);
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    store.reserve_operation(id(1)).unwrap();
    let before = fixture.bytes();
    assert!(store.archive_terminal(id(1)).is_err());
    assert_eq!(fixture.bytes(), before);
    drop(store);
    let fixture = Fixture::present();
    fixture.install(test_stored_record(id(1), true), false);
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    store.reserve_operation(id(1)).unwrap();
    let before = fixture.bytes();
    assert!(store.publish(&test_stored_record(id(1), false)).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn any_defined_unfinished_temporary_file_blocks_without_cleanup() {
    for bytes in [Vec::new(), vec![b'x'; MAX_RECORD as usize]] {
        let fixture = Fixture::present();
        fixture.write(&fixture.root().join(format!("{}.tmp", id(1))), &bytes);
        let before = fixture.bytes();
        assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn full_active_and_archive_inventories_are_readable_but_admission_is_write_zero() {
    for archived in [false, true] {
        let fixture = Fixture::present();
        let limit = if archived { MAX_ARCHIVE } else { MAX_ACTIVE };
        for number in 1..=limit {
            fixture.install(test_stored_record(id(number as u64), true), archived);
        }
        let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
        assert_eq!(store.records().count(), limit);
        let before = fixture.bytes();
        assert!(store.reserve_operation(id(2000)).is_err());
        assert_eq!(fixture.bytes(), before);
    }
}

#[test]
fn count_and_byte_overflow_are_refused_before_any_publication() {
    assert!(
        capacity(
            MAX_ACTIVE,
            MAX_ACTIVE_BYTES,
            MAX_TMP,
            MAX_TMP_BYTES,
            MAX_ARCHIVE,
            MAX_ARCHIVE_BYTES
        )
        .is_ok()
    );
    for values in [
        (MAX_ACTIVE + 1, 0, 0, 0, 0, 0),
        (0, MAX_ACTIVE_BYTES + 1, 0, 0, 0, 0),
        (0, 0, MAX_TMP + 1, 0, 0, 0),
        (0, 0, 0, MAX_TMP_BYTES + 1, 0, 0),
        (0, 0, 0, 0, MAX_ARCHIVE + 1, 0),
        (0, 0, 0, 0, 0, MAX_ARCHIVE_BYTES + 1),
    ] {
        assert!(capacity(values.0, values.1, values.2, values.3, values.4, values.5).is_err());
    }
    let fixture = Fixture::present();
    for number in 1..=MAX_ACTIVE + 1 {
        fixture.install(test_stored_record(id(number as u64), true), false);
    }
    let before = fixture.bytes();
    assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn individual_file_bound_accepts_exact_limit_and_rejects_one_byte_more() {
    let fixture = Fixture::present();
    let exact = fixture.state.join("exact");
    fixture.write(&exact, &vec![b'x'; MAX_RECORD as usize]);
    let file = OpenOptions::new().read(true).open(&exact).unwrap();
    assert_eq!(
        bounded_bytes(&file, MAX_RECORD).unwrap().len(),
        MAX_RECORD as usize
    );
    let over = fixture.state.join("over");
    fixture.write(&over, &vec![b'x'; MAX_RECORD as usize + 1]);
    let file = OpenOptions::new().read(true).open(&over).unwrap();
    assert!(bounded_bytes(&file, MAX_RECORD).is_err());
}

#[test]
fn publication_interruptions_preserve_every_artifact_and_never_auto_repair() {
    for point in 1..=9 {
        let fixture = if point <= 4 {
            Fixture::empty()
        } else {
            Fixture::present()
        };
        let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
        store.reserve_operation(id(1)).unwrap();
        store.test_fail_once_at(point);
        assert!(store.publish(&test_stored_record(id(1), false)).is_err());
        drop(store);
        let before = fixture.bytes();
        let _ = C08IntentStoreV1::inspect(&fixture.state);
        assert_eq!(fixture.bytes(), before);
        if (5..=7).contains(&point) {
            assert!(fixture.root().join(format!("{}.tmp", id(1))).exists());
            assert!(C08IntentStoreV1::inspect(&fixture.state).is_err());
        }
        if point >= 8 {
            let reopened = C08IntentStoreV1::inspect(&fixture.state).unwrap();
            assert_eq!(
                reopened.lookup(id(1)).unwrap().canonical_bytes().unwrap(),
                test_stored_record(id(1), false).canonical_bytes().unwrap()
            );
        }
    }
    let fixture = Fixture::present();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    store.reserve_operation(id(1)).unwrap();
    store.test_fail_once_at(13);
    let before = fixture.bytes();
    assert!(store.publish(&test_stored_record(id(1), false)).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn archive_interruptions_reopen_the_same_terminal_record_without_cleanup() {
    for point in 10..=12 {
        let fixture = Fixture::present();
        fixture.install(test_stored_record(id(1), true), false);
        let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
        store.reserve_operation(id(1)).unwrap();
        store.test_fail_once_at(point);
        assert!(store.archive_terminal(id(1)).is_err());
        drop(store);
        let before = fixture.bytes();
        let reopened = C08IntentStoreV1::inspect(&fixture.state).unwrap();
        assert_eq!(
            reopened.lookup(id(1)).unwrap().canonical_bytes().unwrap(),
            test_stored_record(id(1), true).canonical_bytes().unwrap()
        );
        assert_eq!(fixture.bytes(), before);
        assert!(!fixture.root().join(format!("{}.json", id(1))).exists());
        assert!(fixture.archive().join(format!("{}.json", id(1))).exists());
    }
}

#[test]
fn fresh_terminal_history_cannot_skip_before_write_publication() {
    let fixture = Fixture::empty();
    let mut store = C08IntentStoreV1::inspect(&fixture.state).unwrap();
    store.reserve_operation(id(1)).unwrap();
    let before = fixture.bytes();
    assert!(store.publish(&test_stored_record(id(1), true)).is_err());
    assert_eq!(fixture.bytes(), before);
}
