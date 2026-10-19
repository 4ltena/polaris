//! Transport storage-root discovery and retained-inventory regression tests.
use super::*;
use crate::{desktop_store::SessionManagerStore, session_manager::SessionId};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
const A: &str = "0000000000000001";
const B: &str = "0000000000000002";
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    temp
}
fn data(path: &Path) {
    directory(path);
    directory(&path.join("state"));
}
fn project(root: &Path, id: &str) -> PathBuf {
    let path = root.join("state").join(id);
    directory(&path);
    path
}
fn paths(view: &TransportRootDiscoveryV2) -> Vec<PathBuf> {
    view.verified_roots()
        .unwrap()
        .iter()
        .map(|r| r.path().to_owned())
        .collect()
}

#[test]
fn discovery_includes_every_project_in_all_data_roots_and_exact_settings_siblings() {
    let temp = fixture();
    let default = temp.path().join("default");
    let custom = temp.path().join("custom");
    data(&default);
    data(&custom);
    let mut expected = vec![default.clone(), custom.clone()];
    for root in [&default, &custom] {
        for id in [A, B] {
            let path = project(root, id);
            expected.push(path.clone());
            expected.push(path.join("tui-admissions-v1"));
            fs::write(path.join("unrelated-settings"), b"unread payload").unwrap();
        }
    }
    let settings = [
        temp.path().join("settings.json"),
        temp.path().join("other.settings"),
    ];
    for path in &settings {
        fs::write(path, b"not decoded by topology inspection").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        expected.push(path.with_file_name(format!(
            "{}.workspace",
            path.file_name().unwrap().to_str().unwrap()
        )));
    }
    let view =
        TransportRootDiscoveryV2::inspect(&[default.clone(), custom, default], &settings).unwrap();
    expected.sort();
    assert_eq!(paths(&view), expected);
    assert!(
        view.roots
            .iter()
            .filter(|r| matches!(r, TransportRootV2::Desktop(_)))
            .all(|r| !r.path().exists())
    );
    view.verify_current().unwrap();
}

#[test]
fn discovery_connects_every_project_family_to_one_retained_owner() {
    let temp = fixture();
    let root = temp.path().join("data");
    data(&root);
    let mut families = Vec::new();
    for id in [A, B] {
        let path = project(&root, id).join("tui-admissions-v1");
        directory(&path);
        SessionManagerStore::open_family_owned(&path, &SessionId::new(id).unwrap()).unwrap();
        families.push((path, id));
    }
    let settings = temp.path().join("settings.json");
    let desktop = temp.path().join("settings.json.workspace");
    directory(&desktop);
    SessionManagerStore::open_owned(&desktop).unwrap();
    let discovery = TransportRootDiscoveryV2::inspect(&[root], &[settings]).unwrap();
    let view = TransportRootsObservationV2::inspect(discovery.verified_roots().unwrap()).unwrap();
    discovery.verify_current().unwrap();
    for (path, id) in &families {
        assert!(matches!(
            SessionManagerStore::open_family_owned(path, &SessionId::new(*id).unwrap()),
            Err(StoreError::Busy)
        ));
    }
    assert!(matches!(
        SessionManagerStore::open_owned(&desktop),
        Err(StoreError::Busy)
    ));
    drop(view);
    for (path, id) in &families {
        SessionManagerStore::open_family_owned(path, &SessionId::new(*id).unwrap()).unwrap();
    }
    SessionManagerStore::open_owned(&desktop).unwrap();
}

#[test]
fn discovery_rejects_unknown_project_names_files_aliases_and_unsafe_modes() {
    for kind in 0..4 {
        let temp = fixture();
        let root = temp.path().join("data");
        data(&root);
        let path = root
            .join("state")
            .join(if kind == 0 { "unknown" } else { A });
        match kind {
            0 => directory(&path),
            1 => fs::write(&path, b"not a project directory").unwrap(),
            2 => {
                let target = temp.path().join("target");
                directory(&target);
                symlink(&target, &path).unwrap();
            }
            _ => {
                directory(&path);
                fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
            }
        }
        assert!(TransportRootDiscoveryV2::inspect(&[root], &[]).is_err());
        assert!(path.symlink_metadata().is_ok());
    }
}

#[test]
fn discovery_closes_state_project_and_family_namespace_drift() {
    for kind in 0..4 {
        let temp = fixture();
        let root = temp.path().join("data");
        data(&root);
        let current = project(&root, A);
        let view = TransportRootDiscoveryV2::inspect(&[root.clone()], &[]).unwrap();
        match kind {
            0 => {
                project(&root, B);
            }
            1 => {
                fs::rename(root.join("state"), root.join("retained-state")).unwrap();
                directory(&root.join("state"));
            }
            2 => {
                fs::rename(&current, root.join("state").join(B)).unwrap();
            }
            _ => directory(&current.join("tui-admissions-v1")),
        }
        assert!(view.verified_roots().is_err());
    }
}

#[test]
fn discovery_absent_data_state_and_desktop_are_read_only_and_rechecked() {
    for kind in 0..3 {
        let temp = fixture();
        let root = temp.path().join("data");
        if kind != 0 {
            directory(&root);
        }
        let settings = temp.path().join("settings.json");
        let before = file_stamp(&temp.path().metadata().unwrap());
        let view = TransportRootDiscoveryV2::inspect(&[root.clone()], &[settings]).unwrap();
        assert_eq!(file_stamp(&temp.path().metadata().unwrap()), before);
        match kind {
            0 => directory(&root),
            1 => directory(&root.join("state")),
            _ => directory(&temp.path().join("settings.json.workspace")),
        }
        assert!(view.verify_current().is_err());
    }
}

#[test]
fn discovery_closes_late_project_addition_before_return() {
    let temp = fixture();
    let root = temp.path().join("data");
    data(&root);
    project(&root, A);
    let changed = root.clone();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_DISCOVERY.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            project(&changed, B);
            hit.set(true);
        }))
    });
    assert!(TransportRootDiscoveryV2::inspect(&[root], &[]).is_err());
    assert!(reached.get());
}

#[test]
fn discovery_closes_addition_immediately_after_namespace_eof() {
    let temp = fixture();
    let root = temp.path().join("data");
    data(&root);
    let current = project(&root, A);
    let view = TransportRootDiscoveryV2::inspect(&[root], &[]).unwrap();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_NAMESPACE_NAMES.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            fs::write(current.join("late-sibling"), b"not ignored").unwrap();
            hit.set(true);
        }))
    });
    assert!(view.verify_current().is_err());
    assert!(reached.get());
}

#[test]
fn discovery_closes_project_change_after_outer_state_verification() {
    let temp = fixture();
    let root = temp.path().join("data");
    data(&root);
    let current = project(&root, A);
    let view = TransportRootDiscoveryV2::inspect(&[root], &[]).unwrap();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_DISCOVERY_VERIFY.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            directory(&current.join("tui-admissions-v1"));
            hit.set(true);
        }))
    });
    assert!(view.verify_current().is_err());
    assert!(reached.get());
}

#[test]
fn discovery_rejects_invalid_or_alias_settings_locations_without_reading_settings() {
    let temp = fixture();
    let root = temp.path().join("data");
    directory(&root);
    let alias = temp.path().join("alias");
    symlink(temp.path(), &alias).unwrap();
    for settings in [
        PathBuf::from("relative/settings.json"),
        alias.join("settings.json"),
        PathBuf::from(format!("{}/settings.json/", temp.path().display())),
    ] {
        assert!(TransportRootDiscoveryV2::inspect(&[root.clone()], &[settings]).is_err());
    }
    assert!(TransportRootDiscoveryV2::inspect(&[], &[]).is_err());
}
#[test]
fn discovery_name_budget_is_cumulative_across_projects_and_data_roots() {
    let temp = fixture();
    let one = temp.path().join("one");
    data(&one);
    let two = temp.path().join("two");
    data(&two);
    for root in [&one, &two] {
        let current = project(root, A);
        for name in ["first", "second"] {
            fs::write(current.join(name), b"unread").unwrap();
        }
    }
    // Each root alone uses 1 project name + 2 child names. Six is the exact
    // combined cap; a per-project or per-data-root reset would wrongly pass 5.
    TransportRootDiscoveryV2::inspect_bounded(&[one.clone(), two.clone()], &[], 6).unwrap();
    assert!(TransportRootDiscoveryV2::inspect_bounded(&[one, two], &[], 5).is_err());
}
