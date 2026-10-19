//! Management retirement, global staging and partial-evidence recovery tests.
use super::super::super::test_fixture::GuardFixture;
use super::*;
use crate::conversation_state::{ConversationStateV2, ConversationStore};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};
const ID: &str = "00000000-0000-4000-8000-000000000001";
fn management_fixture() -> (tempfile::TempDir, MigrationAdmissionV2, Vec<PathBuf>) {
    management_fixture_history(false)
}
pub(super) fn management_fixture_history(
    populated: bool,
) -> (tempfile::TempDir, MigrationAdmissionV2, Vec<PathBuf>) {
    use crate::management_effect::*;
    use crate::project_mutation::ProjectMutationOwnerV1;
    use std::str::FromStr;
    let fixture = GuardFixture::new_for_current_coordinator(&"a".repeat(64));
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let parent = home.join(".polaris");
    directory(&parent.join("mcp"));
    directory(&parent.join("mcp/servers"));
    directory(&parent.join("state"));
    let mut states = Vec::new();
    for index in 0..2 {
        let project = home.join(format!("project-{index}"));
        directory(&project);
        directory(&project.join(".polaris"));
        let state = parent
            .join("state")
            .join(crate::project::project_id(&project));
        directory(&state);
        let owner = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = owner.try_mutation().unwrap();
        let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        if populated {
            for generation in 0..2 {
                let request = PrepareManagementEffectV1 {
                    operation_id: ManagementOperationIdV1::from_str(
                        &if generation == 0 { "1" } else { "2" }.repeat(32),
                    )
                    .unwrap(),
                    inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
                    scope: ManagementEffectScopeV1 {
                        project: store.project_scope_hash(),
                        session: Some(ManagementEffectHashV1::digest(b"session")),
                        generation: generation + 1,
                        revision: 1,
                        permission: ManagementPermissionScopeV1::Project,
                        account_binding: None,
                    },
                    request_hash: ManagementEffectHashV1::digest(b"request"),
                    rewind: ManagementRewindDispositionV1::IrreversibleLocal,
                };
                let PrepareManagementEffectOutcomeV1::Prepared(prepared) = store
                    .prepare(&lease, store.cursor().unwrap(), request)
                    .unwrap()
                else {
                    panic!("new request");
                };
                let started = store.start(&lease, &prepared).unwrap();
                store
                    .complete_started(&lease, &started, ManagementEffectHashV1::digest(b"receipt"))
                    .unwrap();
                if generation == 0 {
                    store.test_joined_seal(&lease).unwrap();
                }
            }
        } else {
            store.test_joined_seal(&lease).unwrap();
        }
        let meta = fs::metadata(&project).unwrap();
        let namespace = state.join("rewind-workspace/namespace.json");
        fs::write(&namespace, serde_json::to_vec(&serde_json::json!({"version":1,"project_root":project,"root_device":meta.dev(),"root_inode":meta.ino()})).unwrap()).unwrap();
        fs::set_permissions(namespace, fs::Permissions::from_mode(0o600)).unwrap();
        states.push(state);
    }
    let source = SourcePreflightV2::inspect(&home).unwrap();
    let discovery = TransportRootDiscoveryV2::inspect(&[parent], &[]).unwrap();
    let admission = MigrationAdmissionV2::assemble(source, fixture.guard, discovery).unwrap();
    (fixture._root, admission, states)
}

#[test]
fn management_physical_retirement_holds_all_owners_and_two_phase_native_closure() {
    let (_fixture, admission, states) = management_fixture();
    let retired = admission
        .retire_source()
        .unwrap()
        .retire_management_projects()
        .unwrap();
    retired.verify_current().unwrap();
    assert!(
        retired
            .admission
            .roots
            .management_paths()
            .unwrap()
            .is_empty()
    );
    let mut locks = Vec::new();
    for state in &states {
        let source = state.join(format!("management-effects-v1.retired-{}", "a".repeat(64)));
        assert!(source.join("generations/1").is_dir());
        let tombstone: TombstoneV2 =
            serde_json::from_slice(&fs::read(state.join("management-effects-v1")).unwrap())
                .unwrap();
        assert_eq!(
            tombstone.retired_source_anchor,
            EntryAnchorV2::of(&fs::metadata(&source).unwrap())
        );
        assert_eq!(tombstone.migration_id, "a".repeat(64));
        for path in [
            source.join("owner.lock"),
            state.join("rewind-workspace/writer.lease"),
        ] {
            let file = File::open(path).unwrap();
            assert!(matches!(
                file.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            locks.push(file);
        }
        assert!(!state.join("management-effects-v2").exists());
    }
    drop(retired);
    for file in locks {
        file.try_lock().unwrap();
    }
}

#[test]
fn management_retirement_connects_to_global_staging_without_activating_project_stores() {
    let (_fixture, admission, states) = management_fixture_history(true);
    let staged = admission.retire_source().unwrap().stage_source().unwrap();
    staged.verify_current().unwrap();
    for state in states {
        assert!(state.join("management-effects-v1").is_file());
        let retired = state.join(format!("management-effects-v1.retired-{}", "a".repeat(64)));
        assert!(retired.join("generations/1").is_dir());
        for path in [
            retired.join("owner.lock"),
            state.join("rewind-workspace/writer.lease"),
        ] {
            assert!(matches!(
                File::open(path).unwrap().try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        }
        assert!(!state.join("management-effects-v2").exists());
        let stage = state.join(format!("management-effects-v2.staging-{}", "a".repeat(64)));
        assert!(stage.join("CURRENT").is_file());
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(stage.join("migration.v2.json")).unwrap()).unwrap();
        assert_eq!(manifest["generations"], 2);
        assert_eq!(manifest["effects"], 2);
        assert_eq!(manifest["records"], 6);
    }
}

#[test]
fn management_staging_faults_preserve_partial_tree_and_reject_changed_payload() {
    for point in [
        "before-management-staging-payload",
        "after-management-staging-payload",
        "after-management-staging-readback",
    ] {
        let (_fixture, admission, mut states) = management_fixture();
        states.sort();
        let staged = states[0].join(format!("management-effects-v2.staging-{}", "a".repeat(64)));
        let damaged = staged.clone();
        let retired = admission
            .retire_source()
            .unwrap()
            .retire_management_projects()
            .unwrap();
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let hit = reached.clone();
        FAULT.with(|slot| {
            *slot.borrow_mut() = Some((
                point,
                Box::new(move || {
                    hit.set(true);
                    if point == "after-management-staging-payload" {
                        fs::write(damaged.join("CURRENT"), b"foreign payload").unwrap();
                        Ok(())
                    } else {
                        Err(Error::Storage)
                    }
                }),
            ))
        });
        let result = retired.stage_management_projects();
        if point == "after-management-staging-payload" {
            assert!(matches!(result, Err(Error::Protection)));
        } else {
            assert!(matches!(result, Err(Error::Storage)));
        }
        assert!(reached.get(), "{point}");
        assert!(staged.is_dir());
        for state in states {
            assert!(!state.join("management-effects-v2").exists());
            assert!(
                state
                    .join(format!("management-effects-v1.retired-{}", "a".repeat(64)))
                    .is_dir()
            );
            assert!(state.join("management-effects-v1").is_file());
        }
    }
}

#[test]
fn management_physical_retirement_faults_preserve_partial_evidence_without_activation() {
    for point in [
        "before-management-rename",
        "after-management-rename",
        "after-management-rename-fsync",
        "before-management-tombstone",
        "after-management-tombstone-create",
        "after-management-tombstone-file-fsync",
        "after-management-tombstone-parent-fsync",
        "after-management-retired",
    ] {
        let (_fixture, admission, states) = management_fixture();
        let retired = admission.retire_source().unwrap();
        let hit = std::rc::Rc::new(std::cell::Cell::new(false));
        let reached = hit.clone();
        FAULT.with(|slot| {
            *slot.borrow_mut() = Some((
                point,
                Box::new(move || {
                    reached.set(true);
                    Err(Error::Storage)
                }),
            ))
        });
        assert!(
            matches!(retired.retire_management_projects(), Err(Error::Storage)),
            "{point}"
        );
        assert!(hit.get(), "{point}");
        for state in states {
            assert!(!state.join("management-effects-v2").exists());
            assert!(
                state.join("management-effects-v1").exists()
                    || state
                        .join(format!("management-effects-v1.retired-{}", "a".repeat(64)))
                        .is_dir()
            );
        }
    }
}

#[test]
fn management_physical_retirement_rejects_external_old_path_without_overwrite() {
    let (_fixture, admission, mut states) = management_fixture();
    states.sort();
    let old = states[0].join("management-effects-v1");
    let foreign = old.clone();
    let retired = admission.retire_source().unwrap();
    FAULT.with(|slot| {
        *slot.borrow_mut() = Some((
            "before-management-tombstone",
            Box::new(move || {
                fs::write(&foreign, b"external writer").unwrap();
                fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
                Ok(())
            }),
        ))
    });
    assert!(matches!(
        retired.retire_management_projects(),
        Err(Error::Protection)
    ));
    assert_eq!(fs::read(old).unwrap(), b"external writer");
    for state in states {
        assert!(!state.join("management-effects-v2").exists());
    }
}
#[test]
fn retained_physical_retirement_prepares_source_without_advancing_phase_or_releasing_locks() {
    let (_root, admission, session, parent) = fixture(true);
    let retired = admission.retire_source().unwrap();
    let intent_before = retired.intent.record().clone();
    let payload = retired.prepare_source_payload().unwrap();
    payload.verify_current().unwrap();
    assert_eq!(retired.intent.record(), &intent_before);
    assert!(matches!(
        intent_before,
        ExternalMigrationIntentV2::Retired { .. }
    ));
    assert!(session.lock().is_err());
    assert!(!parent.join("mcp-v2").exists());
    assert!(
        !parent
            .join(format!("mcp-v2.staging-{}", "a".repeat(64)))
            .exists()
    );
    drop(payload);
    retired.verify_current().unwrap();
    drop(retired);
    session.lock().unwrap();
}
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
pub(super) fn fixture(
    owner_lock: bool,
) -> (
    tempfile::TempDir,
    MigrationAdmissionV2,
    ConversationStore,
    PathBuf,
) {
    let fixture = GuardFixture::new_for_current_coordinator(&"a".repeat(64));
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let parent = home.join(".polaris");
    directory(&parent.join("mcp"));
    directory(&parent.join("mcp/servers"));
    if owner_lock {
        fs::write(parent.join("mcp/owner.lock"), b"").unwrap();
        fs::set_permissions(
            parent.join("mcp/owner.lock"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let source = SourcePreflightV2::inspect(&home).unwrap();
    let session = ConversationStore::create(
        &parent,
        ConversationStateV2::new("project".into(), ID.into()).unwrap(),
    )
    .unwrap();
    let discovery = TransportRootDiscoveryV2::inspect(&[parent.clone()], &[]).unwrap();
    let admission = MigrationAdmissionV2::assemble(source, fixture.guard, discovery).unwrap();
    (fixture._root, admission, session, parent)
}
#[test]
fn physical_retirement_persists_actual_anchor_chain_and_retains_all_locks() {
    for has_lock in [false, true] {
        let (_root, admission, session, parent) = fixture(has_lock);
        let source_inode = admission.source.held_root().metadata().unwrap().ino();
        let retired = admission.retire_source().unwrap();
        retired.verify_current().unwrap();
        let ExternalMigrationIntentV2::Retired { record } = retired.intent.record() else {
            panic!("retired")
        };
        assert_eq!(record.retired_source.anchor.inode, source_inode);
        assert!(record.binding.stop.inspected_processes > 0);
        record.binding.stop.coordinator.verify().unwrap();
        assert_eq!(record.binding.source_store_identity, None);
        assert_eq!(record.binding.store_identity.len(), 64);
        let tombstone: TombstoneV2 =
            serde_json::from_slice(&fs::read(parent.join("mcp")).unwrap()).unwrap();
        assert_eq!(tombstone.protocol_version, 2);
        assert_eq!(tombstone.migration_id, record.binding.migration_id);
        assert_eq!(
            tombstone.retired_source_anchor,
            record.retired_source.anchor
        );
        assert_eq!(
            parent
                .join(&record.binding.layout.retired.component())
                .metadata()
                .unwrap()
                .ino(),
            source_inode
        );
        let reopened = ExternalIntentFileV2::open(&retired.admission.guard).unwrap();
        assert_eq!(reopened.record(), retired.intent.record());
        let lock = has_lock.then(|| {
            File::open(
                parent
                    .join(record.binding.layout.retired.component())
                    .join("owner.lock"),
            )
            .unwrap()
        });
        if let Some(lock) = &lock {
            assert!(matches!(
                lock.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        }
        assert!(session.lock().is_err());
        assert!(!parent.join("mcp-v2").exists());
        retired.admission.guard.verify_migrating().unwrap();
        drop(retired);
        session.lock().unwrap();
        if let Some(lock) = lock {
            lock.try_lock().unwrap();
        }
        assert!(parent.join("mcp").is_file());
    }
}
#[test]
fn physical_retirement_busy_source_lock_writes_no_intent_or_retired_root() {
    let (_root, admission, _session, parent) = fixture(true);
    let lock = File::open(parent.join("mcp/owner.lock")).unwrap();
    lock.try_lock().unwrap();
    assert!(admission.retire_source().is_err());
    assert!(parent.join("mcp").is_dir());
    assert!(
        !parent
            .join(format!("mcp.retired-{}", "a".repeat(64)))
            .exists()
    );
    assert!(
        !parent
            .join(format!("mcp-migration.v2-{}.json", "a".repeat(64)))
            .exists()
    );
}
#[test]
fn physical_retirement_faults_preserve_migrating_source_and_durable_evidence() {
    for point in [
        "after-intent",
        "before-rename",
        "after-rename",
        "after-rename-fsync",
        "after-tombstone-create",
        "after-tombstone-write",
        "after-tombstone-file-fsync",
        "after-tombstone",
        "after-retired-intent",
    ] {
        let (_root, admission, session, parent) = fixture(true);
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let hit = reached.clone();
        FAULT.with(|slot| {
            *slot.borrow_mut() = Some((
                point,
                Box::new(move || {
                    hit.set(true);
                    Err(Error::Storage)
                }),
            ))
        });
        assert!(
            matches!(admission.retire_source(), Err(Error::Storage)),
            "{point}"
        );
        assert!(reached.get(), "{point}");
        let head = parent.join(format!("mcp-migration.v2-{}.json", "a".repeat(64)));
        let record = ExternalMigrationIntentV2::decode(&fs::read(head).unwrap()).unwrap();
        assert!(matches!(
            record,
            ExternalMigrationIntentV2::RetirePrepared { .. }
                | ExternalMigrationIntentV2::Retired { .. }
        ));
        let retired = parent.join(format!("mcp.retired-{}", "a".repeat(64)));
        assert!(parent.join("mcp").is_dir() || retired.is_dir());
        assert!(!parent.join("mcp-v2").exists());
        session.lock().unwrap();
        assert!(
            polaris_mcp::entry_gate::ProductionEntryLeaseV2::enter(
                parent.parent().unwrap(),
                &std::env::current_exe().unwrap()
            )
            .is_err()
        );
    }
}
#[test]
fn physical_retirement_competing_old_path_is_preserved_without_tombstone_overwrite() {
    let (_root, admission, _session, parent) = fixture(true);
    let target = parent.join("mcp");
    FAULT.with(|slot| {
        *slot.borrow_mut() = Some((
            "after-rename",
            Box::new(move || {
                fs::write(target, b"competing writer").unwrap();
                Ok(())
            }),
        ))
    });
    assert!(admission.retire_source().is_err());
    assert_eq!(fs::read(parent.join("mcp")).unwrap(), b"competing writer");
    assert!(
        parent
            .join(format!("mcp.retired-{}", "a".repeat(64)))
            .is_dir()
    );
    assert!(!parent.join("mcp-v2").exists());
}
#[test]
fn physical_retirement_closes_tombstone_intent_and_retired_owner_drift() {
    for mutation in 0..3 {
        let (_root, admission, _session, parent) = fixture(true);
        let retired = admission.retire_source().unwrap();
        let target = match mutation {
            0 => parent.join("mcp"),
            1 => parent.join(format!("mcp-migration.v2-{}.json", "a".repeat(64))),
            _ => parent
                .join(format!("mcp.retired-{}", "a".repeat(64)))
                .join("owner.lock"),
        };
        let bytes = fs::read(&target).unwrap();
        fs::write(target, bytes).unwrap();
        assert!(retired.verify_current().is_err(), "{mutation}");
        assert!(!parent.join("mcp-v2").exists());
    }
}

#[test]
fn tombstone_admits_actual_mount_before_writing_any_body() {
    use super::super::intent_file::MOUNT_CHECK;
    let fixture = GuardFixture::new(&"a".repeat(64));
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let parent = home.join(".polaris");
    let source = parent.join(format!("mcp.retired-{}", "a".repeat(64)));
    directory(&source);
    let anchor = EntryAnchorV2::of(&source.metadata().unwrap());
    let parent_inode = fixture.guard.parent.metadata().unwrap().ino();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    MOUNT_CHECK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |parent, file| {
            assert_eq!(parent.metadata().unwrap().ino(), parent_inode);
            assert!(file.metadata().unwrap().is_file());
            assert_eq!(file.metadata().unwrap().len(), 0);
            hit.set(true);
            Err(Error::Protection)
        }))
    });
    assert!(matches!(
        TombstonePin::create(
            &fixture.guard.parent,
            TombstoneV2 {
                protocol_version: 2,
                migration_id: "a".repeat(64),
                retired_source_anchor: anchor,
            }
        ),
        Err(Error::Protection)
    ));
    assert!(reached.get());
    assert_eq!(fs::read(parent.join("mcp")).unwrap(), b"");
    assert!(source.is_dir());
    fixture.guard.verify_migrating().unwrap();
}
