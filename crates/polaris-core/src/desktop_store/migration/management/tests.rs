//! Management-store migration ownership and terminal-state inspection tests.
use super::*;
use crate::management_effect::*;
use std::{
    cell::Cell,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    rc::Rc,
    str::FromStr,
};
struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    data: PathBuf,
    state: PathBuf,
    owner: ProjectMutationOwnerV1,
    lease: Option<ProjectMutationLeaseV1>,
    store: ManagementEffectStoreV1,
}
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let project = root.path().join("project");
        directory(&project);
        directory(&project.join(".polaris"));
        let data = root.path().join("data");
        directory(&data);
        directory(&data.join("state"));
        let state = data
            .join("state")
            .join(crate::project::project_id(&project));
        directory(&state);
        let owner = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = owner.try_mutation().unwrap();
        let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        Self {
            _root: root,
            project,
            data,
            state,
            owner,
            lease: Some(lease),
            store,
        }
    }
    fn namespace(&self) {
        let meta = fs::metadata(&self.project).unwrap();
        let path = self.state.join("rewind-workspace/namespace.json");
        fs::write(&path,serde_json::to_vec(&serde_json::json!({"version":1,"project_root":self.project,"root_device":meta.dev(),"root_inode":meta.ino()})).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn terminal(&self) {
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::from_str(&"1".repeat(32)).unwrap(),
            inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: self.store.project_scope_hash(),
                session: Some(ManagementEffectHashV1::digest(b"session")),
                generation: 1,
                revision: 1,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(b"request"),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let lease = self.lease.as_ref().unwrap();
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = self
            .store
            .prepare(lease, self.store.cursor().unwrap(), request)
            .unwrap()
        else {
            panic!("new")
        };
        let started = self.store.start(lease, &prepared).unwrap();
        self.store
            .complete_started(lease, &started, ManagementEffectHashV1::digest(b"receipt"))
            .unwrap();
    }
    fn release(&mut self) {
        self.lease.take();
    }
}
#[test]
fn management_keeps_project_and_history_owners_without_writes() {
    let mut f = Fixture::new();
    f.namespace();
    f.terminal();
    f.release();
    let journal = f.state.join("management-effects-v1/journal.v1.jsonl");
    let before = fs::read(&journal).unwrap();
    let view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
    view.verify_current().unwrap();
    assert!(view.live_project.is_some());
    assert!(view.history.is_some());
    assert!(matches!(
        f.owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    let lock = fs::File::open(f.state.join("management-effects-v1/owner.lock")).unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert_eq!(fs::read(journal).unwrap(), before);
    assert!(!f.state.join("management-effects-v2").exists());
    drop(view);
    f.owner.try_mutation().unwrap();
    lock.try_lock().unwrap();
}

#[test]
fn management_retired_rebind_keeps_original_history_and_lock() {
    let mut f = Fixture::new();
    f.namespace();
    f.terminal();
    f.store.test_joined_seal(f.lease.as_ref().unwrap()).unwrap();
    f.release();
    let mut view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
    let history = view.history.take().unwrap();
    history.verify_read_snapshot().unwrap();
    let id = ManagementEffectHashV1::digest(b"retirement");
    let source = f.state.join("management-effects-v1");
    let retired = f
        .state
        .join(format!("management-effects-v1.retired-{}", id.as_hex()));
    let bytes = fs::read(source.join("journal.v1.jsonl")).unwrap();
    let identity = fs::metadata(&source).unwrap();
    fs::rename(&source, &retired).unwrap();
    let rebound = history
        .rebind_retired_for_migration(&view.state.ancestor, id)
        .unwrap();
    rebound.verify_read_snapshot().unwrap();
    assert_eq!(fs::read(retired.join("journal.v1.jsonl")).unwrap(), bytes);
    let fresh = fs::metadata(&retired).unwrap();
    assert_eq!((fresh.dev(), fresh.ino()), (identity.dev(), identity.ino()));
    assert!(matches!(
        f.owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    let lock = fs::File::open(retired.join("owner.lock")).unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(!f.state.join("management-effects-v2").exists());
    assert!(
        rebound
            .rebind_retired_for_migration(&view.state.ancestor, id)
            .is_err()
    );
    lock.try_lock().unwrap();
}

#[test]
fn management_retired_rebind_rejects_foreign_binding_and_original_pin_drift() {
    for kind in 0..9 {
        let mut f = Fixture::new();
        f.namespace();
        f.terminal();
        f.store.test_joined_seal(f.lease.as_ref().unwrap()).unwrap();
        f.release();
        let mut view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
        let history = view.history.take().unwrap();
        let id = ManagementEffectHashV1::digest(b"retirement");
        let source = f.state.join("management-effects-v1");
        let retired = f
            .state
            .join(format!("management-effects-v1.retired-{}", id.as_hex()));
        fs::rename(&source, &retired).unwrap();
        let mut requested_id = id;
        match kind {
            0 => requested_id = ManagementEffectHashV1::from_hex(&"0".repeat(64)).unwrap(),
            1 => requested_id = ManagementEffectHashV1::digest(b"foreign"),
            2 => {
                fs::rename(&retired, f.state.join("foreign-root")).unwrap();
                directory(&retired);
            }
            3 => {
                let journal = retired.join("journal.v1.jsonl");
                let bytes = fs::read(&journal).unwrap();
                fs::write(journal, bytes).unwrap();
            }
            4 => {
                let lock = retired.join("owner.lock");
                fs::rename(&lock, f.state.join("original-owner.lock")).unwrap();
                fs::write(&lock, b"").unwrap();
                fs::set_permissions(lock, fs::Permissions::from_mode(0o600)).unwrap();
            }
            5 => directory(&retired.join("foreign-directory")),
            6 => fs::set_permissions(&retired, fs::Permissions::from_mode(0o750)).unwrap(),
            7 => {
                let root = fs::File::open(&retired).unwrap();
                root.set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
                    .unwrap();
            }
            8 => {
                let late = retired.join("generations/1/late");
                fs::write(&late, b"late").unwrap();
                fs::remove_file(late).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            history
                .rebind_retired_for_migration(&view.state.ancestor, requested_id)
                .is_err(),
            "case {kind}"
        );
        assert!(!f.state.join("management-effects-v2").exists());
    }
}

#[test]
fn management_retired_rebind_rejects_replacement_state_even_with_original_root() {
    let mut f = Fixture::new();
    f.namespace();
    f.terminal();
    f.release();
    let mut view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
    let history = view.history.take().unwrap();
    let id = ManagementEffectHashV1::digest(b"retirement");
    let name = format!("management-effects-v1.retired-{}", id.as_hex());
    fs::rename(f.state.join("management-effects-v1"), f.state.join(&name)).unwrap();
    let original = f.state.with_file_name("original-state");
    fs::rename(&f.state, &original).unwrap();
    directory(&f.state);
    fs::rename(original.join(&name), f.state.join(&name)).unwrap();
    let replacement = crate::project_mutation::open_directory_nofollow(&f.state).unwrap();
    assert!(
        history
            .rebind_retired_for_migration(&replacement, id)
            .is_err()
    );
    assert!(view.verify_current().is_err());
    assert!(!f.state.join("management-effects-v2").exists());
}

fn fixture_retire_management(
    f: &Fixture,
    view: &ManagementProjectObservationV2,
    id: ManagementEffectHashV1,
) -> PathBuf {
    let source = f.state.join("management-effects-v1");
    let retired = f
        .state
        .join(format!("management-effects-v1.retired-{}", id.as_hex()));
    fs::rename(&source, &retired).unwrap();
    let tombstone = ManagementTombstoneV2 {
        protocol_version: 2,
        migration_id: id.as_hex(),
        retired_source_anchor: EntryAnchorV2::of(&view.source.ancestor.metadata().unwrap()),
    };
    fs::write(&source, serde_json::to_vec(&tombstone).unwrap()).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    retired
}

#[test]
fn management_phase_rebind_keeps_project_lease_and_exact_retired_closure() {
    let mut f = Fixture::new();
    f.namespace();
    f.terminal();
    f.store.test_joined_seal(f.lease.as_ref().unwrap()).unwrap();
    f.release();
    let view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
    let id = ManagementEffectHashV1::digest(b"retirement");
    let retired = fixture_retire_management(&f, &view, id);
    let view = view.rebind_retired(id).unwrap();
    view.verify_current().unwrap();
    view.runtime_exclusions().unwrap();
    assert_eq!(view.source.path, retired);
    assert!(matches!(
        f.owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    let lock = fs::File::open(retired.join("owner.lock")).unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(!f.state.join("management-effects-v2").exists());
    let tombstone = f.state.join("management-effects-v1");
    let bytes = fs::read(&tombstone).unwrap();
    fs::write(&tombstone, bytes).unwrap();
    assert!(view.verify_current().is_err());
    drop(view);
    f.owner.try_mutation().unwrap();
    lock.try_lock().unwrap();
}

#[test]
fn management_phase_rebind_rejects_other_children_workspace_and_tombstone_changes() {
    for kind in 0..7 {
        let mut f = Fixture::new();
        f.namespace();
        f.terminal();
        let unrelated = f.state.join("unrelated");
        fs::write(&unrelated, b"original").unwrap();
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o600)).unwrap();
        f.release();
        let view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
        let id = ManagementEffectHashV1::digest(b"retirement");
        fixture_retire_management(&f, &view, id);
        match kind {
            0 => fs::write(unrelated, b"original").unwrap(),
            1 => directory(&f.state.join("foreign")),
            2 => {
                let path = f.state.join("rewind-workspace/namespace.json");
                let bytes = fs::read(&path).unwrap();
                fs::write(path, bytes).unwrap();
            }
            3 => {
                let path = f.state.join("rewind-workspace/writer.lease");
                fs::rename(&path, f.state.join("old-writer.lease")).unwrap();
                fs::write(&path, b"").unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            4 => {
                let path = f.state.join("management-effects-v1");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["migration_id"] =
                    serde_json::json!(ManagementEffectHashV1::digest(b"foreign").as_hex());
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            5 => fs::set_permissions(
                f.state.join("management-effects-v1"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap(),
            6 => fs::hard_link(
                f.state.join("management-effects-v1"),
                f._root.path().join("alias"),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        assert!(view.rebind_retired(id).is_err(), "case {kind}");
        assert!(!f.state.join("management-effects-v2").exists());
    }
}
#[test]
fn management_namespace_can_bind_empty_or_detached_historical_projects() {
    for kind in 0..3 {
        let mut f = Fixture::new();
        f.namespace();
        f.release();
        if kind > 0 {
            fs::rename(&f.project, f._root.path().join("old-project")).unwrap();
            if kind == 2 {
                directory(&f.project);
            }
        }
        let view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
        view.verify_current().unwrap();
        assert_eq!(view.live_project.is_some(), kind == 0);
        assert!(view.history.is_some());
        assert_eq!(view.historical.as_ref().unwrap().project_root, f.project);
    }
}
#[test]
fn management_absent_namespace_uses_unique_history_but_rejects_unbound_empty() {
    let mut f = Fixture::new();
    f.release();
    assert!(ManagementProjectObservationV2::inspect(&f.state).is_err());
    assert!(!f.state.join("rewind-workspace/namespace.json").exists());
    f.lease = Some(f.owner.try_mutation().unwrap());
    f.terminal();
    f.release();
    let view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
    assert!(view.historical.is_none());
    assert!(view.live_project.is_none());
    view.verify_current().unwrap();
}
#[test]
fn management_missing_store_and_workspace_are_observed_without_creation() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let state = root.path().join("0000000000000001");
    directory(&state);
    let before = file_stamp(&state.metadata().unwrap());
    let view = ManagementProjectObservationV2::inspect(&state).unwrap();
    view.verify_current().unwrap();
    assert_eq!(fs::read_dir(&state).unwrap().count(), 0);
    assert_eq!(file_stamp(&state.metadata().unwrap()), before);
    directory(&state.join("management-effects-v1"));
    assert!(view.verify_current().is_err());
}
#[test]
fn management_rejects_namespace_mismatch_unsafe_wire_and_bound_history_disagreement() {
    for kind in 0..4 {
        let mut f = Fixture::new();
        f.namespace();
        f.terminal();
        let path = f.state.join("rewind-workspace/namespace.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match kind {
            0 => value["version"] = serde_json::json!(2),
            1 => value["project_root"] = serde_json::json!("/foreign/project"),
            2 => value["project_root"] = serde_json::json!("relative"),
            _ => value["root_inode"] = serde_json::json!(0),
        }
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        f.release();
        assert!(
            ManagementProjectObservationV2::inspect(&f.state).is_err(),
            "{kind}"
        );
    }
}
#[test]
fn management_closes_namespace_lease_body_and_source_changes() {
    for kind in 0..4 {
        let mut f = Fixture::new();
        f.namespace();
        f.terminal();
        f.release();
        let view = ManagementProjectObservationV2::inspect(&f.state).unwrap();
        let path = match kind {
            0 => f.state.join("rewind-workspace/namespace.json"),
            1 => f.state.join("rewind-workspace/writer.lease"),
            2 => f.state.join("management-effects-v1/journal.v1.jsonl"),
            _ => f.state.join("rewind-workspace/late"),
        };
        fs::write(&path, b"changed").unwrap();
        assert!(view.verify_current().is_err(), "{kind}");
        assert!(matches!(
            f.owner.try_mutation(),
            Err(crate::project_mutation::ProjectMutationError::Busy)
        ));
    }
}
#[test]
fn management_closes_changes_during_history_and_final_checks() {
    for kind in 0..2 {
        let mut f = Fixture::new();
        f.namespace();
        f.terminal();
        f.release();
        let path = f.state.join("rewind-workspace/namespace.json");
        let reached = Rc::new(Cell::new(false));
        let hit = reached.clone();
        let hook: Box<dyn FnOnce()> = Box::new(move || {
            let bytes = fs::read(&path).unwrap();
            fs::write(path, bytes).unwrap();
            hit.set(true);
        });
        if kind == 0 {
            AFTER_MANAGEMENT_HISTORY.with(|s| *s.borrow_mut() = Some(hook));
        } else {
            AFTER_MANAGEMENT_VERIFY.with(|s| *s.borrow_mut() = Some(hook));
        }
        assert!(ManagementProjectObservationV2::inspect(&f.state).is_err());
        assert!(reached.get());
        f.owner.try_mutation().unwrap();
    }
}
#[test]
fn management_discovery_includes_all_project_owners_and_releases_on_later_failure() {
    let mut f = Fixture::new();
    f.terminal();
    f.release();
    let discovery =
        super::super::discovery::TransportRootDiscoveryV2::inspect(&[f.data.clone()], &[]).unwrap();
    let view = TransportRootsObservationV2::inspect(discovery.verified_roots().unwrap()).unwrap();
    assert!(matches!(
        f.owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    view.verify_current().unwrap();
    drop(view);
    f.owner.try_mutation().unwrap();
    let later = f._root.path().join("z-desktop");
    directory(&later);
    fs::write(later.join("foreign"), b"retained").unwrap();
    assert!(
        TransportRootsObservationV2::inspect(&[
            TransportRootV2::ManagementProject(f.state.clone()),
            TransportRootV2::Desktop(later)
        ])
        .is_err()
    );
    f.owner.try_mutation().unwrap();
}
