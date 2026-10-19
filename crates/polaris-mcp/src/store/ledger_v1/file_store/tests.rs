//! Ledger file-store identity, persistence and namespace regression tests.
use super::super::{
    CredentialLedgerDescriptorV1, CredentialLedgerInventoryV1, CredentialLedgerPlanV1,
    CredentialLedgerStepKindV1, CredentialLedgerStepV1, MAX_LEDGER_OPERATIONS, MAX_LEDGER_RECORDS,
    RetiredLedgerAnchorV1, TombstoneLedgerAnchorV1, operation_reference_hash,
};
use super::*;
use crate::store::documents_v2::{DocumentKindV2, DocumentMetadataV2, DocumentPresenceV2};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
};

fn hex(byte: char) -> String {
    byte.to_string().repeat(64)
}
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn new_file(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn contents(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn names(path: &Path) -> BTreeSet<String> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

// Every operation uses this owned disposable home. No environment HOME,
// production gate, existing Keychain or real credential document is consulted.
struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    parent: PathBuf,
    root: PathBuf,
    ledger: PathBuf,
    epoch: LedgerEpochV1,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("home");
        directory(&home);
        let parent = home.join(".polaris");
        directory(&parent);
        let root = parent.join(ROOT);
        directory(&root);
        let retired = parent.join("owned-retired");
        directory(&retired);
        let tombstone = parent.join("owned-tombstone");
        new_file(&tombstone, b"owned non-secret migration marker");
        new_file(&parent.join(LOCK), b"");
        let root_fd = physical_directory(&root).unwrap();
        prepare_empty_payload(&root_fd, &hex('2')).unwrap();
        let ledger = root.join(LEDGER);
        let epoch = LedgerEpochV1 {
            protocol_version: 2,
            store_identity: hex('1'),
            random_epoch_id: hex('2'),
            migration_id: hex('3'),
            canonical_parent_anchor: EntryAnchorV2::of(&fs::metadata(&parent).unwrap()),
            tombstone_anchor: TombstoneLedgerAnchorV1 {
                anchor: EntryAnchorV2::of(&fs::metadata(&tombstone).unwrap()),
                file_hash: bytes_hash(&fs::read(&tombstone).unwrap()),
            },
            retired_source_anchor: RetiredLedgerAnchorV1 {
                anchor: EntryAnchorV2::of(&fs::metadata(&retired).unwrap()),
                non_secret_tree_hash: hex('5'),
            },
            active_v2_root_anchor: EntryAnchorV2::of(&root_fd.metadata().unwrap()),
            activation_tree_hash: hex('6'),
        };
        new_file(&ledger.join(EPOCH), &epoch.encode().unwrap());
        physical_directory(&ledger).unwrap().sync_all().unwrap();
        root_fd.sync_all().unwrap();
        physical_directory(&parent).unwrap().sync_all().unwrap();
        Self {
            _temp: temp,
            home,
            parent,
            root,
            ledger,
            epoch,
        }
    }
    fn inspect(&self, mode: LedgerOpenModeV1) -> Result<CredentialLedgerFileStoreV1, Error> {
        CredentialLedgerFileStoreV1::inspect(&self.parent, &self.epoch, mode)
    }
    fn writer(&self) -> CredentialLedgerFileStoreV1 {
        self.inspect(LedgerOpenModeV1::Mutating).unwrap()
    }
    fn assert_blocked_without_repair(&self) {
        let before_names = names(&self.ledger);
        let before: Vec<_> = before_names
            .iter()
            .map(|name| {
                let path = self.ledger.join(name);
                (
                    name.clone(),
                    fs::symlink_metadata(&path).unwrap(),
                    fs::read(path).ok(),
                )
            })
            .collect();
        assert!(self.inspect(LedgerOpenModeV1::ReadOnly).is_err());
        assert_eq!(names(&self.ledger), before_names);
        for (name, metadata, bytes) in before {
            let path = self.ledger.join(name);
            assert_eq!(
                EntryAnchorV2::of(&fs::symlink_metadata(&path).unwrap()),
                EntryAnchorV2::of(&metadata)
            );
            assert_eq!(fs::read(path).ok(), bytes);
        }
    }
}

fn descriptor() -> CredentialLedgerDescriptorV1 {
    let references = vec!["owned_ref".to_owned()];
    let reference_hash = operation_reference_hash(&references).unwrap();
    CredentialLedgerDescriptorV1 {
        references: references.clone(),
        operation_id: format!("{:032x}", 1),
        origin_project_identity: hex('9'),
        store_identity: hex('1'),
        inventory: CredentialLedgerInventoryV1::C24,
        root_operation_id: format!("{:032x}", 1),
        predecessor_operation_id: None,
        chain_depth: 1,
        plan_hash: hex('a'),
        operation_reference_set_hash: reference_hash.clone(),
        authorization_anchor_hash: hex('b'),
        credential_before: DocumentMetadataV2 {
            kind: DocumentKindV2::Credential,
            presence: DocumentPresenceV2::Present,
            revision: 4,
            token: hex('7'),
            receipt_hash: hex('8'),
        },
        plan: CredentialLedgerPlanV1::Single {
            step: CredentialLedgerStepV1 {
                references,
                kind: CredentialLedgerStepKindV1::Set,
                operation_reference_set_hash: reference_hash,
            },
        },
    }
}
fn reserved() -> CredentialLedgerEventV1 {
    CredentialLedgerEventV1::Reserved {
        descriptor: descriptor(),
    }
}
fn armed() -> CredentialLedgerEventV1 {
    CredentialLedgerEventV1::Armed {
        operation_id: descriptor().operation_id,
        project_authorization_anchor_hash: hex('b'),
    }
}

#[test]
fn empty_staging_payload_precedes_epoch_without_projection_cycle_or_internal_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("owned-staging");
    directory(&root);
    let fd = physical_directory(&root).unwrap();
    prepare_empty_payload(&fd, &hex('2')).unwrap();
    assert_eq!(names(&root), BTreeSet::from([LEDGER.to_owned()]));
    assert_eq!(
        names(&root.join(LEDGER)),
        BTreeSet::from([HISTORY.to_owned(), HEAD.to_owned()])
    );
    assert_eq!(fs::read(root.join(LEDGER).join(HISTORY)).unwrap(), b"");
    let bytes = fs::read(root.join(LEDGER).join(HEAD)).unwrap();
    let head = LedgerHeadV1::decode(&bytes).unwrap();
    assert_eq!(head.sequence, 0);
    assert_eq!(head.ledger_epoch_id, hex('2'));
    assert_eq!(head.last_record_hash, None);
    assert_eq!(head.history_hash, bytes_hash(b""));
    assert!(!String::from_utf8(bytes).unwrap().contains("epoch_hash"));
    assert!(!root.join(LEDGER).join(EPOCH).exists());
}

#[test]
fn staging_factory_rejects_invalid_id_or_existing_payload_without_repair() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("owned-staging");
    directory(&root);
    let fd = physical_directory(&root).unwrap();
    assert_eq!(prepare_empty_payload(&fd, "bad"), Err(Error::Protocol));
    assert!(names(&root).is_empty());
    prepare_empty_payload(&fd, &hex('2')).unwrap();
    let before = fs::read(root.join(LEDGER).join(HEAD)).unwrap();
    assert!(prepare_empty_payload(&fd, &hex('3')).is_err());
    assert_eq!(fs::read(root.join(LEDGER).join(HEAD)).unwrap(), before);
}

#[test]
fn existing_empty_ledger_is_fully_read_back_and_does_not_create_entries() {
    let mut fixture = Fixture::new();
    // Canonical parent/home may be owned 0755; only v2/ledger are private 0700.
    for path in [&fixture.home, &fixture.parent] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fixture.epoch.canonical_parent_anchor =
        EntryAnchorV2::of(&fs::metadata(&fixture.parent).unwrap());
    contents(
        &fixture.ledger.join(EPOCH),
        &fixture.epoch.encode().unwrap(),
    );
    let before = names(&fixture.parent);
    let reader = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.sequence, 0);
    assert_eq!(snapshot.epoch, fixture.epoch);
    assert!(
        snapshot
            .operation_is_absent(&descriptor().operation_id)
            .unwrap()
    );
    assert_eq!(names(&fixture.parent), before);
    assert_eq!(
        names(&fixture.ledger),
        BTreeSet::from([EPOCH.to_owned(), HISTORY.to_owned(), HEAD.to_owned()])
    );
}

#[test]
fn required_file_and_root_absence_never_become_empty_or_create_replacements() {
    for name in [LOCK, EPOCH, HISTORY, HEAD] {
        let fixture = Fixture::new();
        let parent = if name == LOCK {
            &fixture.parent
        } else {
            &fixture.ledger
        };
        fs::remove_file(parent.join(name)).unwrap();
        let before = names(parent);
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "{name}"
        );
        assert_eq!(names(parent), before);
        assert!(!parent.join(name).exists());
    }
    let fixture = Fixture::new();
    fs::rename(&fixture.root, fixture.parent.join("owned-moved-root")).unwrap();
    let before = names(&fixture.parent);
    assert_eq!(
        fixture.inspect(LedgerOpenModeV1::ReadOnly).err(),
        Some(Error::RecoveryRequired)
    );
    assert_eq!(names(&fixture.parent), before);
    assert!(!fixture.root.exists());
    let fixture = Fixture::new();
    fs::rename(&fixture.parent, fixture.home.join("owned-moved-parent")).unwrap();
    assert!(fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err());
    assert!(!fixture.parent.exists());
}

#[test]
fn append_requires_exclusive_reservation_and_returns_only_read_back_record() {
    let fixture = Fixture::new();
    let mut writer = fixture.writer();
    assert_eq!(writer.append(reserved()), Err(Error::Limit));
    assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), b"");
    writer.reserve_capacity(2, 1).unwrap();
    let record = writer.append(reserved()).unwrap();
    assert_eq!(record.sequence, 1);
    let mut encoded = record.encode().unwrap();
    encoded.push(b'\n');
    assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), encoded);
    assert_eq!(writer.snapshot().unwrap().head_hash, record.record_hash);
    let head = LedgerHeadV1::decode(&fs::read(fixture.ledger.join(HEAD)).unwrap()).unwrap();
    assert_eq!(
        head.last_record_hash.as_deref(),
        Some(record.record_hash.as_str())
    );
    assert_eq!(head.history_bytes as usize, encoded.len());
    assert_eq!(head.history_hash, bytes_hash(&encoded));
    let armed_record = writer.append(armed()).unwrap();
    assert_eq!(armed_record.sequence, 2);
    assert_eq!(writer.append(armed()), Err(Error::Limit));
    assert!(!fixture.ledger.join(HEAD_TEMP).exists());
    drop(writer);
    let reopened = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
    assert_eq!(reopened.snapshot().unwrap().sequence, 2);
    assert_eq!(
        reopened.snapshot().unwrap().head_hash,
        armed_record.record_hash
    );
}

#[test]
fn read_only_shared_and_mutating_exclusive_locks_conflict_and_drop_releases() {
    let fixture = Fixture::new();
    let mut first = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
    let second = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
    assert_eq!(first.reserve_capacity(1, 1), Err(Error::Protection));
    assert_eq!(first.append(reserved()), Err(Error::Protection));
    assert_eq!(
        fixture.inspect(LedgerOpenModeV1::Mutating).err(),
        Some(Error::RevisionConflict)
    );
    drop(first);
    drop(second);
    let exclusive = fixture.writer();
    for mode in [LedgerOpenModeV1::ReadOnly, LedgerOpenModeV1::Mutating] {
        assert_eq!(fixture.inspect(mode).err(), Some(Error::RevisionConflict));
    }
    drop(exclusive);
    assert_eq!(
        fixture
            .inspect(LedgerOpenModeV1::ReadOnly)
            .unwrap()
            .snapshot()
            .unwrap()
            .sequence,
        0
    );
}

#[test]
fn capacity_rejection_is_prewrite_and_does_not_destroy_existing_reservation() {
    let fixture = Fixture::new();
    let mut writer = fixture.writer();
    writer.reserve_capacity(1, 1).unwrap();
    let before = names(&fixture.ledger);
    for (records, operations) in [
        (0, 0),
        (1, 2),
        (MAX_LEDGER_RECORDS, 0),
        (MAX_LEDGER_OPERATIONS + 1, MAX_LEDGER_OPERATIONS + 1),
        (usize::MAX, 0),
    ] {
        assert_eq!(
            writer.reserve_capacity(records, operations),
            Err(Error::Limit)
        );
        assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), b"");
        assert_eq!(names(&fixture.ledger), before);
        assert_eq!(writer.snapshot().unwrap().sequence, 0);
    }
    assert_eq!(writer.append(reserved()).unwrap().sequence, 1);
}

#[test]
fn operation_reservation_and_invalid_transition_fail_before_any_write() {
    let fixture = Fixture::new();
    let mut writer = fixture.writer();
    writer.reserve_capacity(2, 0).unwrap();
    assert_eq!(writer.append(reserved()), Err(Error::Limit));
    assert_eq!(writer.append(armed()), Err(Error::Protocol));
    assert!(!writer.poisoned);
    assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), b"");
    writer.reserve_capacity(1, 1).unwrap();
    writer.append(reserved()).unwrap();
}

#[test]
fn epoch_is_canonical_immutable_and_compared_to_checked_expected_epoch() {
    let fixture = Fixture::new();
    let mut wrong = fixture.epoch.clone();
    wrong.random_epoch_id = hex('f');
    assert!(
        CredentialLedgerFileStoreV1::inspect(&fixture.parent, &wrong, LedgerOpenModeV1::ReadOnly)
            .is_err()
    );
    wrong.random_epoch_id = "invalid".to_owned();
    assert_eq!(
        CredentialLedgerFileStoreV1::inspect(&fixture.parent, &wrong, LedgerOpenModeV1::ReadOnly)
            .err(),
        Some(Error::Protocol)
    );
    let mut bytes = fixture.epoch.encode().unwrap();
    bytes.push(b' ');
    contents(&fixture.ledger.join(EPOCH), &bytes);
    fixture.assert_blocked_without_repair();
}

#[test]
fn head_rejects_unknown_fields_versions_noncanonical_bytes_and_inconsistent_empty_state() {
    let changes: [fn(&mut LedgerHeadV1); 6] = [
        |v| {
            v.schema_version = 2;
        },
        |v| {
            v.ledger_epoch_id = hex('f');
        },
        |v| {
            v.sequence = 1;
        },
        |v| {
            v.last_record_hash = Some(hex('f'));
        },
        |v| {
            v.history_bytes = 1;
        },
        |v| {
            v.history_hash = hex('f');
        },
    ];
    for change in changes {
        let fixture = Fixture::new();
        let mut value =
            LedgerHeadV1::decode(&fs::read(fixture.ledger.join(HEAD)).unwrap()).unwrap();
        change(&mut value);
        // Keep struct field order canonical so each rejected case tests the
        // actual head binding, rather than an unrelated serializer ordering.
        contents(&fixture.ledger.join(HEAD), &value.encode().unwrap());
        fixture.assert_blocked_without_repair();
    }
    let fixture = Fixture::new();
    let mut bytes = fs::read(fixture.ledger.join(HEAD)).unwrap();
    bytes.push(b'\n');
    contents(&fixture.ledger.join(HEAD), &bytes);
    fixture.assert_blocked_without_repair();
    let fixture = Fixture::new();
    let mut bytes = fs::read(fixture.ledger.join(HEAD)).unwrap();
    bytes.pop();
    bytes.extend_from_slice(b",\"unknown\":true}");
    contents(&fixture.ledger.join(HEAD), &bytes);
    fixture.assert_blocked_without_repair();
    let fixture = Fixture::new();
    let bytes = String::from_utf8(fs::read(fixture.ledger.join(HEAD)).unwrap())
        .unwrap()
        .replace(",\"last_record_hash\":null", "");
    contents(&fixture.ledger.join(HEAD), bytes.as_bytes());
    fixture.assert_blocked_without_repair();
}

#[test]
fn valid_prefix_truncation_is_detected_even_when_codec_alone_accepts_prefix() {
    let fixture = Fixture::new();
    let mut writer = fixture.writer();
    writer.reserve_capacity(2, 1).unwrap();
    let first = writer.append(reserved()).unwrap();
    writer.append(armed()).unwrap();
    drop(writer);
    let mut prefix = first.encode().unwrap();
    prefix.push(b'\n');
    assert_eq!(
        CredentialLedgerSnapshotV1::decode(&fixture.epoch.encode().unwrap(), &prefix)
            .unwrap()
            .sequence,
        1
    );
    contents(&fixture.ledger.join(HISTORY), &prefix);
    fixture.assert_blocked_without_repair();
}

#[test]
fn torn_tail_complete_history_with_old_head_and_head_temp_are_preserved_blockers() {
    let fixture = Fixture::new();
    let snapshot =
        CredentialLedgerSnapshotV1::decode(&fixture.epoch.encode().unwrap(), b"").unwrap();
    let record = snapshot.next_record(reserved()).unwrap().encode().unwrap();
    contents(&fixture.ledger.join(HISTORY), &record[..record.len() / 2]);
    fixture.assert_blocked_without_repair();
    let fixture = Fixture::new();
    let snapshot =
        CredentialLedgerSnapshotV1::decode(&fixture.epoch.encode().unwrap(), b"").unwrap();
    let mut record = snapshot.next_record(reserved()).unwrap().encode().unwrap();
    record.push(b'\n');
    contents(&fixture.ledger.join(HISTORY), &record);
    fixture.assert_blocked_without_repair();
    let fixture = Fixture::new();
    new_file(
        &fixture.ledger.join(HEAD_TEMP),
        &fs::read(fixture.ledger.join(HEAD)).unwrap(),
    );
    fixture.assert_blocked_without_repair();
}

#[test]
fn bound_epoch_history_head_and_global_lock_inode_replacements_fail_closed() {
    for name in [EPOCH, HISTORY, HEAD, LOCK] {
        let fixture = Fixture::new();
        let mut writer = fixture.writer();
        writer.reserve_capacity(1, 1).unwrap();
        let path = if name == LOCK {
            fixture.parent.join(name)
        } else {
            fixture.ledger.join(name)
        };
        let before = fs::read(&path).unwrap();
        let replacement = fixture.home.join("owned-replacement");
        new_file(&replacement, &before);
        fs::rename(replacement, &path).unwrap();
        assert!(writer.snapshot().is_err(), "{name}");
        assert!(writer.append(reserved()).is_err(), "{name}");
        assert!(writer.poisoned);
        assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), b"");
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

#[test]
fn ledger_root_parent_and_home_directory_replacements_break_ancestry_pins() {
    for target in ["ledger", "root", "parent", "home"] {
        let fixture = Fixture::new();
        let reader = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
        let path = match target {
            "ledger" => &fixture.ledger,
            "root" => &fixture.root,
            "parent" => &fixture.parent,
            _ => &fixture.home,
        };
        let moved = path.with_file_name(format!("owned-{target}-moved"));
        fs::rename(path, moved).unwrap();
        directory(path);
        assert!(reader.snapshot().is_err(), "{target}");
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "{target}"
        );
        assert!(names(path).is_empty(), "inspector changed {target}");
    }
}

#[test]
fn unsafe_modes_hardlinks_symlinks_special_files_and_unknown_entries_block_inspection() {
    for name in [EPOCH, HISTORY, HEAD, LOCK] {
        let fixture = Fixture::new();
        let path = if name == LOCK {
            fixture.parent.join(name)
        } else {
            fixture.ledger.join(name)
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "mode {name}"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = fixture.home.join("owned-hardlink");
        fs::hard_link(&path, &alias).unwrap();
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "link {name}"
        );
        fs::remove_file(&alias).unwrap();
        fs::rename(&path, &alias).unwrap();
        symlink(&alias, &path).unwrap();
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "symlink {name}"
        );
    }
    for name in ["owned-unknown", "head.pending", "owned-special"] {
        let fixture = Fixture::new();
        let path = fixture.ledger.join(name);
        if name == "owned-special" {
            let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        } else {
            new_file(&path, b"");
        }
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "{name}"
        );
        assert!(fixture.ledger.join(name).exists());
    }
}

#[test]
fn unsafe_directory_modes_and_symlink_traversal_are_rejected_without_repair() {
    for target in ["home", "parent", "root", "ledger"] {
        let fixture = Fixture::new();
        let path = match target {
            "home" => &fixture.home,
            "parent" => &fixture.parent,
            "root" => &fixture.root,
            _ => &fixture.ledger,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "{target}"
        );
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o777);
    }
    let fixture = Fixture::new();
    let moved = fixture.home.join("owned-parent-alias");
    fs::rename(&fixture.parent, &moved).unwrap();
    symlink(&moved, &fixture.parent).unwrap();
    assert!(fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err());
    let aliased = fixture.parent.join(".");
    assert!(
        CredentialLedgerFileStoreV1::inspect(&aliased, &fixture.epoch, LedgerOpenModeV1::ReadOnly)
            .is_err()
    );
}

#[test]
fn lock_nonempty_contents_and_after_open_mode_changes_invalidate_handles() {
    let fixture = Fixture::new();
    contents(&fixture.parent.join(LOCK), b"unexpected");
    assert_eq!(
        fixture.inspect(LedgerOpenModeV1::ReadOnly).err(),
        Some(Error::Protection)
    );
    let fixture = Fixture::new();
    let reader = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
    fs::set_permissions(
        fixture.ledger.join(HISTORY),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    assert!(reader.snapshot().is_err());
}

#[test]
fn history_and_head_content_drift_after_open_are_revalidated_on_snapshot_and_append() {
    for name in [EPOCH, HISTORY, HEAD] {
        let fixture = Fixture::new();
        let mut writer = fixture.writer();
        writer.reserve_capacity(1, 1).unwrap();
        let path = fixture.ledger.join(name);
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(b' ');
        contents(&path, &bytes);
        assert!(writer.snapshot().is_err(), "{name}");
        assert!(writer.append(reserved()).is_err(), "{name}");
        assert!(writer.poisoned);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn entry_created_after_last_readdir_is_rejected_by_inspect_and_snapshot() {
    for snapshot_probe in [false, true] {
        let fixture = Fixture::new();
        let reader = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
        let late = fixture.ledger.join("owned-late-entry");
        fn inject_at_eof(remaining: usize, owned_late: std::path::PathBuf) {
            INVENTORY_AFTER_EOF.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some(Box::new(move || {
                    if remaining > 1 {
                        inject_at_eof(remaining - 1, owned_late);
                    } else {
                        // Target the final inventory: no later structural pass
                        // can detect this addition on behalf of the EOF check.
                        directory(&owned_late);
                    }
                }));
            });
        }
        // inspect performs its initial inventory plus three verification passes;
        // snapshot performs only those three verification passes.
        inject_at_eof(if snapshot_probe { 3 } else { 4 }, late.clone());
        let result = if snapshot_probe {
            reader.snapshot().map(|_| ())
        } else {
            fixture.inspect(LedgerOpenModeV1::ReadOnly).map(|_| ())
        };
        assert_eq!(result, Err(Error::Protection));
        assert!(late.is_dir(), "the post-readdir hook was not reached");
        INVENTORY_AFTER_EOF.with(|slot| assert!(slot.borrow().is_none()));
        fixture.assert_blocked_without_repair();
        assert!(late.is_dir());
        assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), b"");
    }
}

#[test]
fn bounded_pread_rejects_oversized_files_before_allocating_whole_contents() {
    for (name, maximum) in [
        (EPOCH, MAX_LEDGER_RECORD_BYTES),
        (HISTORY, MAX_LEDGER_BYTES),
        (HEAD, MAX_HEAD_BYTES),
    ] {
        let fixture = Fixture::new();
        OpenOptions::new()
            .write(true)
            .open(fixture.ledger.join(name))
            .unwrap()
            .set_len(maximum as u64 + 1)
            .unwrap();
        assert!(
            fixture.inspect(LedgerOpenModeV1::ReadOnly).is_err(),
            "{name}"
        );
        assert_eq!(
            fs::metadata(fixture.ledger.join(name)).unwrap().len(),
            maximum as u64 + 1
        );
    }
}

#[test]
fn aggregate_budget_counts_current_head_and_reserves_both_replacement_heads() {
    let fixture = Fixture::new();
    let epoch = fixture.epoch.encode().unwrap();
    let head = fs::read(fixture.ledger.join(HEAD)).unwrap();
    let history = vec![b' '; MAX_LEDGER_BYTES - epoch.len() - head.len() + 1];
    assert_eq!(
        decode_payload(&epoch, &history, &head).err(),
        Some(Error::Limit)
    );
    let mut writer = fixture.writer();
    // Synthetic boundary DTO only: reserve itself revalidates the real files.
    // The encoded counter represents existing immutable records; no history
    // bytes are fabricated or installed as durable evidence.
    writer.snapshot.encoded_bytes =
        MAX_LEDGER_BYTES - (MAX_LEDGER_RECORD_BYTES + 1) - MAX_HEAD_BYTES;
    assert_eq!(writer.reserve_capacity(1, 1), Err(Error::Limit));
    assert!(!writer.poisoned);
    assert_eq!(fs::read(fixture.ledger.join(HISTORY)).unwrap(), b"");
}

#[test]
fn zero_write_and_partial_interrupted_writes_are_explicitly_handled() {
    assert_eq!(
        write_all_at_with(b"record", 0, |_, _| Ok(0)),
        Err(Error::Storage)
    );
    let mut bytes = Vec::new();
    let mut interrupted = true;
    write_all_at_with(b"record", 9, |input, offset| {
        if interrupted {
            interrupted = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        assert_eq!(offset, 9 + bytes.len() as u64);
        let count = input.len().min(2);
        bytes.extend_from_slice(&input[..count]);
        Ok(count)
    })
    .unwrap();
    assert_eq!(bytes, b"record");
}

#[test]
fn all_eight_append_faults_poison_handles_and_reopen_as_complete_or_preserved_blocker() {
    for point in 1..=8 {
        let fixture = Fixture::new();
        let mut writer = fixture.writer();
        writer.reserve_capacity(1, 1).unwrap();
        writer.fault = Some(point);
        assert_eq!(
            writer.append(reserved()),
            Err(Error::Storage),
            "point {point}"
        );
        assert_eq!(
            writer.fault_reached.get(),
            Some(point),
            "point {point} was not reached"
        );
        assert!(writer.poisoned);
        writer.fault = None;
        assert_eq!(writer.append(reserved()), Err(Error::RecoveryRequired));
        assert_eq!(writer.reserve_capacity(1, 1), Err(Error::RecoveryRequired));
        assert_eq!(writer.snapshot().err(), Some(Error::RecoveryRequired));
        assert!(!fs::read(fixture.ledger.join(HISTORY)).unwrap().is_empty());
        drop(writer);
        if point <= 6 {
            fixture.assert_blocked_without_repair();
            assert_eq!(fixture.ledger.join(HEAD_TEMP).exists(), point >= 4);
        } else {
            let reopened = fixture.inspect(LedgerOpenModeV1::ReadOnly).unwrap();
            assert_eq!(reopened.snapshot().unwrap().sequence, 1);
            assert!(
                reopened
                    .snapshot()
                    .unwrap()
                    .operations
                    .contains_key(&descriptor().operation_id)
            );
            assert!(!fixture.ledger.join(HEAD_TEMP).exists());
        }
    }
}
