//! Management generation staging, payload validation and migration fault tests.
use super::*;
use crate::management_effect::v2::{ManagementEffectStateV2, ManagementUnknownReasonV2};
use crate::project_mutation::{ProjectMutationLeaseV1, ProjectMutationOwnerV1};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
};

const V1_ROOT: &str = "management-effects-v1";

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn mounted_staging_rejects_every_directory_and_file_before_payload_access() {
    use super::super::super::REJECT_MIGRATION_MOUNT;
    let fixture = Fixture::new();
    fixture.terminal(1, "T10");
    fixture.store.test_joined_seal(&fixture.lease).unwrap();
    fixture.terminal(2, "T10");
    let (mut tree, root, path) = fixture.prepare_tree();
    let limits = ManagementEffectLimitsV1::default();
    let directories = [
        path.clone(),
        path.join(GENERATIONS),
        path.join("generations/1"),
    ];
    for target in &directories {
        let metadata = fs::metadata(target).unwrap();
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(Some((metadata.dev(), metadata.ino()))));
        let result = tree.write_staging_mounted(&root, limits);
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(None));
        assert!(matches!(
            result,
            Err(ManagementEffectStoreError::InvalidStore)
        ));
        assert!(!path.join(CURRENT).exists());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        assert_eq!(fs::read_dir(path.join("generations/1")).unwrap().count(), 0);
    }
    tree.write_staging_mounted(&root, limits).unwrap();
    tree.verify_staging(&root).unwrap();
    let targets = directories
        .into_iter()
        .chain(tree.files.keys().map(|name| path.join(name)));
    for target in targets {
        let metadata = fs::metadata(&target).unwrap();
        // Reject a foreign mount with otherwise valid bytes: a payload-only
        // validator cannot satisfy this assertion.
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(Some((metadata.dev(), metadata.ino()))));
        let result = tree.verify_staging(&root);
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(None));
        assert!(
            matches!(result, Err(ManagementEffectStoreError::InvalidStore)),
            "{}",
            target.display()
        );
        let saved = if metadata.is_file() {
            let bytes = fs::read(&target).unwrap();
            fs::write(&target, b"malformed foreign payload").unwrap();
            Some(bytes)
        } else {
            None
        };
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(Some((metadata.dev(), metadata.ino()))));
        let result = tree.verify_staging(&root);
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(None));
        assert!(
            matches!(result, Err(ManagementEffectStoreError::InvalidStore)),
            "{}",
            target.display()
        );
        if let Some(bytes) = saved {
            fs::write(target, bytes).unwrap();
        }
    }
    tree.verify_staging(&root).unwrap();
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    state: PathBuf,
    _owner: ProjectMutationOwnerV1,
    lease: ProjectMutationLeaseV1,
    store: ManagementEffectStoreV1,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        private_directory(&project);
        private_directory(&project.join(".polaris"));
        private_directory(&state);
        let project = fs::canonicalize(project).unwrap();
        let state = fs::canonicalize(state).unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = owner.try_mutation().unwrap();
        let store = ManagementEffectStoreV1::test_open(
            &state,
            fs::canonicalize(state.join("rewind-workspace")).unwrap(),
            project_hash(&project),
            &lease,
            ManagementEffectLimitsV1::default(),
        )
        .unwrap();
        Self {
            _root: root,
            project,
            state,
            _owner: owner,
            lease,
            store,
        }
    }

    fn request(&self, id: u8, inventory: &str) -> PrepareManagementEffectV1 {
        let mut request = PrepareManagementEffectV1 {
            operation_id: operation_id(id),
            inventory: ManagementInventoryIdV1::parse(inventory).unwrap(),
            scope: ManagementEffectScopeV1 {
                project: self.store.project_scope_hash(),
                session: Some(hash("migration-session")),
                generation: 7,
                revision: 11,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: hash(&format!("migration-request-{id}")),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        if inventory.starts_with('C') || inventory.starts_with('D') {
            request.scope.permission = ManagementPermissionScopeV1::System;
            request.scope.session = None;
        }
        request
    }

    fn prepare(&self, id: u8, inventory: &str) -> PreparedManagementEffectV1 {
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = self
            .store
            .prepare(
                &self.lease,
                self.store.cursor().unwrap(),
                self.request(id, inventory),
            )
            .unwrap()
        else {
            panic!("new effect must be prepared");
        };
        prepared
    }

    fn terminal(&self, id: u8, inventory: &str) {
        let prepared = self.prepare(id, inventory);
        let started = self.store.start(&self.lease, &prepared).unwrap();
        self.store
            .complete_started(&self.lease, &started, hash(&format!("receipt-{id}")))
            .unwrap();
    }

    fn prepare_tree(&self) -> (ManagementMigrationTreeV2, File, PathBuf) {
        let (anchors, staging) = self.migration_anchors();
        let ManagementEffectInspectOpenV1::Present(inspected) = self
            .store
            .test_inspect_migration(&self.lease, ManagementEffectLimitsV1::default())
            .unwrap()
        else {
            panic!("fixture store must exist");
        };
        let tree = inspected
            .prepare_migration_v2(hash("migration-id"), anchors)
            .unwrap();
        (
            tree,
            crate::project_mutation::open_directory_nofollow(&staging).unwrap(),
            staging,
        )
    }

    fn migration_anchors(&self) -> (ManagementMigrationAnchorsV2, PathBuf) {
        let generations = self.store.cursor().unwrap().generation + 1;
        let staging = self.state.join("management-effects-v2-staging");
        private_directory(&staging);
        let staging = fs::canonicalize(staging).unwrap();
        let directories = staging.join(GENERATIONS);
        private_directory(&directories);
        for number in 1..generations {
            private_directory(&directories.join(number.to_string()));
        }
        let tombstone = self.state.join("management-effects-v1.tombstone");
        fs::write(&tombstone, b"retired source").unwrap();
        fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
        (
            ManagementMigrationAnchorsV2 {
                canonical_parent: anchor(&self.state),
                retired_source: anchor(&self.state.join(V1_ROOT)),
                tombstone: anchor(&tombstone),
                active_v2_root: anchor(&staging),
            },
            staging,
        )
    }
}

#[test]
fn retired_root_is_the_only_migration_source_after_tombstoning() {
    let fixture = Fixture::new();
    fixture.terminal(1, "T10");
    let migration_id = hash("retired-migration-id");
    let source = fixture.state.join(V1_ROOT);
    let source_before = inventory(&source);
    let retired = fixture
        .state
        .join(format!("{V1_ROOT}.retired-{}", migration_id.as_hex()));
    fs::rename(&source, &retired).unwrap();
    let tombstone = fixture.state.join(V1_ROOT);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tombstone)
        .unwrap();
    fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
    let tombstone_before = (fs::read(&tombstone).unwrap(), anchor(&tombstone));

    assert!(
        ManagementEffectStoreV1::open_at_state_dir(
            &fixture.project,
            &fixture.state,
            &fixture.lease,
        )
        .is_err()
    );

    let staging = fixture.state.join("management-effects-v2-staging");
    private_directory(&staging);
    let staging = fs::canonicalize(staging).unwrap();
    private_directory(&staging.join(GENERATIONS));
    let anchors = ManagementMigrationAnchorsV2 {
        canonical_parent: anchor(&fixture.state),
        retired_source: anchor(&retired),
        tombstone: anchor(&tombstone),
        active_v2_root: anchor(&staging),
    };
    let ManagementEffectInspectOpenV1::Present(inspected) =
        ManagementEffectStoreV1::inspect_retired_for_migration_at_state_dir(
            &fixture.project,
            &fixture.state,
            migration_id,
            &fixture.lease,
        )
        .unwrap()
    else {
        panic!("retired source must remain inspectable");
    };
    let mut tree = inspected
        .prepare_migration_v2(migration_id, anchors)
        .unwrap();
    let staging_root = crate::project_mutation::open_directory_nofollow(&staging).unwrap();
    tree.write_staging(&staging_root, ManagementEffectLimitsV1::default())
        .unwrap();
    let reopened =
        ManagementMigrationTreeV2::read_staging(&staging_root, ManagementEffectLimitsV1::default())
            .unwrap();
    assert_eq!(reopened.manifest(), tree.manifest());
    assert_eq!(source_before, inventory(&retired));
    assert_eq!(
        tombstone_before,
        (fs::read(&tombstone).unwrap(), anchor(&tombstone))
    );
}

#[test]
fn migrates_legacy_generation_zero_without_changing_pinned_source() {
    let fixture = Fixture::new();
    fixture.terminal(1, "T10");
    let source = inventory(&fixture.state.join(V1_ROOT));
    let (mut tree, staging, _) = fixture.prepare_tree();

    let (effects, sequence) = tree.verify(ManagementEffectLimitsV1::default()).unwrap();
    assert_eq!(sequence, 3);
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects[&operation_id(1)].state,
        ManagementEffectStateV2::Completed { .. }
    ));
    assert_eq!(tree.manifest().generations, 1);
    assert_eq!(tree.manifest().source_current_hash, None);
    assert_eq!(source, inventory(&fixture.state.join(V1_ROOT)));

    tree.write_staging(&staging, ManagementEffectLimitsV1::default())
        .unwrap();
    let reopened =
        ManagementMigrationTreeV2::read_staging(&staging, ManagementEffectLimitsV1::default())
            .unwrap();
    assert_eq!(reopened.manifest(), tree.manifest());
    assert_eq!(
        reopened
            .verify(ManagementEffectLimitsV1::default())
            .unwrap(),
        (effects, sequence)
    );
    assert_eq!(source, inventory(&fixture.state.join(V1_ROOT)));
}

#[test]
fn migrates_all_referenced_generations_and_keeps_legacy_credentials_opaque() {
    let fixture = Fixture::new();
    fixture.terminal(1, "T10");
    fixture.store.test_joined_seal(&fixture.lease).unwrap();
    let credential_ids = [(2, "C24"), (3, "C25"), (4, "C27")];
    for (id, inventory) in credential_ids {
        fixture.terminal(id, inventory);
    }
    let pending = fixture.prepare(8, "T10");
    let started = fixture.store.start(&fixture.lease, &pending).unwrap();
    fixture
        .store
        .mark_outcome_unknown(
            &fixture.lease,
            &started,
            ManagementUnknownReasonV1::DetachedEffect,
        )
        .unwrap();
    for (id, inventory) in [(5, "D32"), (6, "D33"), (7, "C24")] {
        fixture
            .store
            .test_append_legacy_prepared(&fixture.lease, fixture.request(id, inventory))
            .unwrap();
    }
    fixture.store.test_joined_seal(&fixture.lease).unwrap();

    let orphan = fixture
        .state
        .join(V1_ROOT)
        .join(GENERATIONS)
        .join(".orphan-7-1");
    private_directory(&orphan);
    fs::write(orphan.join(JOURNAL_NAME), b"").unwrap();
    fs::set_permissions(orphan.join(JOURNAL_NAME), fs::Permissions::from_mode(0o600)).unwrap();
    let (tree, _, _) = fixture.prepare_tree();
    let (effects, sequence) = tree.verify(ManagementEffectLimitsV1::default()).unwrap();

    assert_eq!(tree.manifest().generations, 3);
    assert_eq!(sequence, tree.manifest().records);
    assert!(
        tree.manifest()
            .event_mappings
            .windows(2)
            .all(|pair| pair[0].destination_sequence + 1 == pair[1].destination_sequence)
    );
    for (id, _) in credential_ids {
        let effect = &effects[&operation_id(id)];
        assert!(matches!(
            effect.credential_authorization,
            Some(CredentialMutationAuthorizationV1::LegacyV1 {
                verified_descriptor: None,
                ..
            })
        ));
        assert!(matches!(
            effect.state,
            ManagementEffectStateV2::Completed { .. }
        ));
    }
    assert!(matches!(
        effects[&operation_id(8)].state,
        ManagementEffectStateV2::OutcomeUnknown {
            reason: ManagementUnknownReasonV2::DetachedEffect
        }
    ));
    for id in [5, 6, 7] {
        let effect = &effects[&operation_id(id)];
        assert!(matches!(
            effect.credential_authorization,
            Some(CredentialMutationAuthorizationV1::LegacyV1 {
                verified_descriptor: None,
                ..
            })
        ));
        assert!(matches!(
            effect.state,
            ManagementEffectStateV2::OutcomeUnknown {
                reason: ManagementUnknownReasonV2::LegacyAuthorizationUnavailable
            }
        ));
    }
    let orphan = tree
        .manifest()
        .source_files
        .iter()
        .find(|file| file.relative_name == "generations/.orphan-7-1/journal.v1.jsonl")
        .unwrap();
    assert!(!orphan.referenced);
    assert!(
        !tree
            .manifest()
            .file_mappings
            .iter()
            .any(|mapping| { mapping.source_name == orphan.relative_name })
    );
}

#[test]
fn verify_rejects_semantic_manifest_mutations_after_rehashing_outer_files() {
    let cases: &[fn(&mut ManagementMigrationTreeV2)] = &[
        |tree| {
            tree.manifest
                .event_mappings
                .push(tree.manifest.event_mappings[0].clone())
        },
        |tree| tree.manifest.file_mappings[0].source_hash = hash("changed-source-hash"),
        |tree| {
            tree.files
                .insert("foreign-v1-and-v2".to_owned(), b"mixed".to_vec());
        },
        |tree| {
            tree.files
                .get_mut(V2_JOURNAL)
                .unwrap()
                .extend_from_slice(b"{\"tail\":");
        },
    ];
    for mutate in cases {
        let fixture = Fixture::new();
        fixture.terminal(1, "T10");
        let (mut tree, _, _) = fixture.prepare_tree();
        mutate(&mut tree);
        rewrite_outer_hashes(&mut tree);
        assert!(tree.verify(ManagementEffectLimitsV1::default()).is_err());
    }
}

#[test]
fn migration_refuses_torn_missing_or_changed_source_evidence() {
    for case in 0..3 {
        let fixture = Fixture::new();
        fixture.terminal(1, "T10");
        let (anchors, _) = fixture.migration_anchors();
        let journal = fixture.store.test_journal_path().to_owned();
        match case {
            0 => {
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&journal)
                    .unwrap()
                    .write_all(b"{\"torn\":")
                    .unwrap();
            }
            1 => fs::remove_file(&journal).unwrap(),
            2 => {
                let mut bytes = fs::read(&journal).unwrap();
                bytes[0] ^= 1;
                fs::write(&journal, bytes).unwrap();
                fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
            }
            _ => unreachable!(),
        }
        let inspected = fixture
            .store
            .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default());
        if case == 0 {
            let ManagementEffectInspectOpenV1::Present(inspected) = inspected.unwrap() else {
                panic!("torn source is still an inspectable source");
            };
            assert!(
                inspected
                    .prepare_migration_v2(hash("migration-id"), anchors)
                    .is_err()
            );
        } else {
            assert!(inspected.is_err());
        }
    }
}

#[test]
fn staged_failures_never_publish_current_before_the_complete_closure() {
    let fixture = Fixture::new();
    fixture.terminal(1, "T10");
    let (tree, _root, _) = fixture.prepare_tree();
    let first_current_point = tree.files.len() as u64 + tree.manifest.generations + 5;
    let last_point = first_current_point + 1;
    for point in 1..=last_point {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let staging_path = root.path().join("staging");
        private_directory(&staging_path);
        let staging_path = fs::canonicalize(staging_path).unwrap();
        private_directory(&staging_path.join(GENERATIONS));
        let tombstone = root.path().join("tombstone");
        fs::write(&tombstone, b"tombstone").unwrap();
        fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
        let mut staged = tree.manifest.clone();
        staged.anchors.active_v2_root = anchor(&staging_path);
        staged.anchors.canonical_parent = anchor(root.path());
        staged.anchors.tombstone = anchor(&tombstone);
        let mut attempt = ManagementMigrationTreeV2 {
            manifest: staged,
            files: tree.files.clone(),
        };
        rewrite_outer_hashes(&mut attempt);
        let directory = crate::project_mutation::open_directory_nofollow(&staging_path).unwrap();
        fail_staging_once_at(point);
        let result = attempt.write_staging(&directory, ManagementEffectLimitsV1::default());
        assert!(
            matches!(&result, Err(ManagementEffectStoreError::Io(error)) if error.to_string() == format!("injected staging failure {point}")),
            "fault point {point}: {result:?}; entries={:?}; actual_anchor={:?}; planned_anchor={:?}",
            fs::read_dir(&staging_path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            anchor(&staging_path),
            attempt.manifest.anchors.active_v2_root
        );
        let current = staging_path.join(CURRENT).exists();
        assert_eq!(
            current,
            point >= first_current_point,
            "fault point {point}: {result:?}"
        );
        assert_eq!(
            ManagementMigrationTreeV2::read_staging(
                &directory,
                ManagementEffectLimitsV1::default()
            )
            .is_ok(),
            point >= first_current_point,
            "fault point {point}",
        );
    }
}

fn rewrite_outer_hashes(tree: &mut ManagementMigrationTreeV2) {
    let migration = encode(&tree.manifest).unwrap();
    tree.files.insert(MIGRATION.to_owned(), migration.clone());
    let mut current: CurrentV2 = decode(&tree.files[CURRENT]).unwrap();
    current.migration_manifest_hash = ManagementEffectHashV1::digest(&migration);
    tree.files
        .insert(CURRENT.to_owned(), encode(&current).unwrap());
}

fn private_directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn anchor(path: &Path) -> ManagementMigrationAnchorV2 {
    let metadata = fs::metadata(path).unwrap();
    ManagementMigrationAnchorV2 {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.mode() & 0o7777,
        links: metadata.nlink(),
    }
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u64, u64)> {
    fn walk(root: &Path, current: &Path, out: &mut BTreeMap<PathBuf, (Vec<u8>, u64, u64)>) {
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    (fs::read(&path).unwrap(), metadata.dev(), metadata.ino()),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn project_hash(project: &Path) -> ManagementEffectHashV1 {
    let project = fs::canonicalize(project).unwrap();
    ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes())
}

fn hash(value: &str) -> ManagementEffectHashV1 {
    ManagementEffectHashV1::digest(value.as_bytes())
}

fn operation_id(value: u8) -> ManagementOperationIdV1 {
    ManagementOperationIdV1::from_str(&format!("{value:032x}")).unwrap()
}

#[test]
fn migration_reader_rejects_busy_owner_without_waiting_or_source_change() {
    let fixture = Fixture::new();
    fixture.terminal(1, "T10");
    let source = fixture.state.join(V1_ROOT);
    let before = inventory(&source);
    let held = fs::File::open(source.join(OWNER_LOCK_NAME)).unwrap();
    held.try_lock().unwrap();
    let result = fixture
        .store
        .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default());
    assert!(
        matches!(result, Err(ManagementEffectStoreError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(inventory(&source), before);
    drop(held);
    fixture
        .store
        .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default())
        .unwrap();
}

#[test]
fn migration_reader_admits_mount_before_root_owner_and_generation_content() {
    use super::super::super::REJECT_MIGRATION_MOUNT;
    for kind in 0..8 {
        let fixture = Fixture::new();
        fixture.terminal(1, "T10");
        fixture.store.test_joined_seal(&fixture.lease).unwrap();
        let source = fixture.state.join(V1_ROOT);
        let candidates = [
            fixture.state.clone(),
            fixture.state.join("rewind-workspace"),
            source.clone(),
            source.join(OWNER_LOCK_NAME),
            source.join(JOURNAL_NAME),
            source.join(GENERATIONS),
            source.join("generations/1"),
            source.join("generations/1/archive.v1.json"),
        ];
        let target = &candidates[kind];
        let meta = fs::metadata(target).unwrap();
        // Poison a regular source file: rejecting its mount must precede decode.
        if meta.is_file() {
            fs::write(target, b"malformed and untrusted\n").unwrap();
        }
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(Some((meta.dev(), meta.ino()))));
        let result = fixture
            .store
            .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default());
        REJECT_MIGRATION_MOUNT.with(|slot| slot.set(None));
        assert!(
            matches!(result, Err(ManagementEffectStoreError::InvalidStore)),
            "{kind}: {result:?}"
        );
    }
}

#[test]
fn migration_reader_closes_census_addition_and_aba_even_when_names_are_restored() {
    use super::super::super::AFTER_MIGRATION_CENSUS;
    for kind in 0..3 {
        let fixture = Fixture::new();
        fixture.terminal(1, "T10");
        fixture.store.test_joined_seal(&fixture.lease).unwrap();
        let source = fixture.state.join(V1_ROOT);
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let hit = reached.clone();
        AFTER_MIGRATION_CENSUS.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let target = if kind == 2 {
                    source.join("generations/1/late")
                } else {
                    source.join("late")
                };
                fs::write(&target, b"late").unwrap();
                if kind != 0 {
                    fs::remove_file(&target).unwrap();
                }
                hit.set(true);
            }));
        });
        let result = fixture
            .store
            .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default());
        assert!(
            matches!(result, Err(ManagementEffectStoreError::InvalidStore)),
            "{kind}: {result:?}"
        );
        assert!(reached.get());
    }
}

#[test]
fn migration_reader_retains_directory_and_file_stamps_for_closing_reinspection() {
    for kind in 0..3 {
        let fixture = Fixture::new();
        fixture.terminal(1, "T10");
        fixture.store.test_joined_seal(&fixture.lease).unwrap();
        let ManagementEffectInspectOpenV1::Present(inspected) = fixture
            .store
            .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default())
            .unwrap()
        else {
            panic!("source exists")
        };
        inspected.verify_read_snapshot().unwrap();
        let source = fixture.state.join(V1_ROOT);
        if kind == 0 {
            let target = source.join(JOURNAL_NAME);
            let same = fs::read(&target).unwrap();
            fs::write(target, same).unwrap();
        } else {
            let target = if kind == 1 {
                source.join("late")
            } else {
                source.join("generations/1/late")
            };
            fs::write(&target, b"late").unwrap();
            fs::remove_file(target).unwrap();
        }
        assert!(inspected.verify_read_snapshot().is_err(), "{kind}");
    }
}

#[test]
fn migration_reader_closes_earlier_pins_after_the_fresh_inventory() {
    use super::super::super::BEFORE_MIGRATION_REFRESH;
    for kind in 0..3 {
        let fixture = Fixture::new();
        fixture.terminal(1, "T10");
        fixture.store.test_joined_seal(&fixture.lease).unwrap();
        let ManagementEffectInspectOpenV1::Present(inspected) = fixture
            .store
            .test_inspect_migration(&fixture.lease, ManagementEffectLimitsV1::default())
            .unwrap()
        else {
            panic!("source exists")
        };
        let source = fixture.state.join(V1_ROOT);
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let hit = reached.clone();
        BEFORE_MIGRATION_REFRESH.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                if kind == 0 {
                    let path = source.join(JOURNAL_NAME);
                    let bytes = fs::read(&path).unwrap();
                    fs::write(path, bytes).unwrap();
                } else {
                    let path = if kind == 1 {
                        source.join("late")
                    } else {
                        source.join("generations/1/late")
                    };
                    fs::write(&path, b"changed").unwrap();
                    fs::remove_file(path).unwrap();
                }
                hit.set(true);
            }))
        });
        assert!(inspected.verify_read_snapshot().is_err(), "{kind}");
        assert!(reached.get());
    }
}

#[test]
fn historical_scope_is_selected_only_from_all_referenced_requests() {
    let f = Fixture::new();
    f.terminal(1, "T10");
    f.store.test_joined_seal(&f.lease).unwrap();
    f.prepare(2, "T10");
    let orphan = f.state.join(V1_ROOT).join("generations/.orphan-7-1");
    private_directory(&orphan);
    fs::write(
        orphan.join(JOURNAL_NAME),
        b"foreign scope is opaque orphan bytes\n",
    )
    .unwrap();
    fs::set_permissions(orphan.join(JOURNAL_NAME), fs::Permissions::from_mode(0o600)).unwrap();
    let before = inventory(&f.state.join(V1_ROOT));
    let ManagementEffectInspectOpenV1::Present(view) =
        ManagementEffectStoreV1::inspect_migration_history(&f.state, &f.lease, None).unwrap()
    else {
        panic!("source")
    };
    view.verify_read_snapshot().unwrap();
    assert_eq!(view.store.project_scope, f.store.project_scope_hash());
    assert_eq!(view.effects().count(), 2);
    assert_eq!(inventory(&f.state.join(V1_ROOT)), before);
}
#[test]
fn historical_scope_rejects_mixed_zero_mismatched_and_empty_sources() {
    for kind in 0..4 {
        let f = Fixture::new();
        if kind != 3 {
            f.terminal(1, "T10");
        }
        if kind < 2 {
            let mut request = f.request(2, "T10");
            request.scope.project = if kind == 0 {
                hash("foreign-project")
            } else {
                ManagementEffectHashV1::zero()
            };
            f.store
                .test_append_legacy_prepared(&f.lease, request)
                .unwrap();
        }
        let expected = if kind == 2 {
            Some(hash("foreign-project"))
        } else {
            None
        };
        let before = inventory(&f.state.join(V1_ROOT));
        assert!(
            ManagementEffectStoreV1::inspect_migration_history(&f.state, &f.lease, expected)
                .is_err(),
            "{kind}"
        );
        assert_eq!(inventory(&f.state.join(V1_ROOT)), before);
    }
}
#[test]
fn historical_scope_handles_empty_first_generation_and_ignores_orphan_only_scope() {
    let f = Fixture::new();
    f.store.test_joined_seal(&f.lease).unwrap();
    f.terminal(1, "T10");
    let ManagementEffectInspectOpenV1::Present(view) =
        ManagementEffectStoreV1::inspect_migration_history(&f.state, &f.lease, None).unwrap()
    else {
        panic!("source")
    };
    assert_eq!(view.store.project_scope, f.store.project_scope_hash());
    drop(view);
    let empty = Fixture::new();
    let orphan = empty.state.join(V1_ROOT).join("generations");
    private_directory(&orphan);
    let orphan = orphan.join(".orphan-7-1");
    private_directory(&orphan);
    fs::copy(
        f.state.join(V1_ROOT).join("generations/1/journal.v1.jsonl"),
        orphan.join(JOURNAL_NAME),
    )
    .unwrap();
    assert!(
        ManagementEffectStoreV1::inspect_migration_history(&empty.state, &empty.lease, None)
            .is_err()
    );
    ManagementEffectStoreV1::inspect_migration_history(
        &empty.state,
        &empty.lease,
        Some(empty.store.project_scope_hash()),
    )
    .unwrap();
}

#[test]
fn historical_scope_rejects_new_prepared_id_reuse_but_keeps_pending_checkpoint_carry() {
    for inventory_id in ["C24", "T10"] {
        let f = Fixture::new();
        f.terminal(1, inventory_id);
        f.store.test_joined_seal(&f.lease).unwrap();
        f.store
            .test_append_legacy_prepared(&f.lease, f.request(1, inventory_id))
            .unwrap();
        let before = inventory(&f.state.join(V1_ROOT));
        for scope in [None, Some(f.store.project_scope_hash())] {
            assert!(
                matches!(
                    ManagementEffectStoreV1::inspect_migration_history(&f.state, &f.lease, scope),
                    Err(ManagementEffectStoreError::CorruptJournal)
                ),
                "{inventory_id}"
            );
        }
        assert_eq!(inventory(&f.state.join(V1_ROOT)), before);
    }
    let f = Fixture::new();
    f.prepare(1, "T10");
    f.store.test_joined_seal(&f.lease).unwrap();
    let prepared = f.store.prepared_effect(operation_id(1)).unwrap().unwrap();
    let started = f.store.start(&f.lease, &prepared).unwrap();
    f.store
        .complete_started(&f.lease, &started, hash("receipt"))
        .unwrap();
    let ManagementEffectInspectOpenV1::Present(view) =
        ManagementEffectStoreV1::inspect_migration_history(&f.state, &f.lease, None).unwrap()
    else {
        panic!("source")
    };
    assert_eq!(view.effects().count(), 1);
    view.verify_read_snapshot().unwrap();
}
