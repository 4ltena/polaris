//! Simultaneous storage-root ownership and migration closure regression tests.
use super::*;
use crate::{
    conversation_state::{ConversationStateV2, ConversationStore},
    desktop_store::SessionManagerStore,
    session_manager::SessionId,
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
const LOCAL_ID: &str = "00000000-0000-4000-8000-000000000001";

fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    temp
}
fn local(path: &Path) -> ConversationStore {
    directory(path);
    ConversationStore::create(
        path,
        ConversationStateV2::new("project".into(), LOCAL_ID.into()).unwrap(),
    )
    .unwrap()
}
fn family(path: &Path) {
    directory(path);
    SessionManagerStore::open_family_owned(path, &SessionId::new("session").unwrap()).unwrap();
}

#[test]
fn root_set_retains_local_and_family_owners_simultaneously() {
    let temp = fixture();
    let a = temp.path().join("a-local");
    let b = temp.path().join("b-family");
    let c = temp.path().join("c-desktop");
    let session = local(&a);
    family(&b);
    directory(&c);
    let manager = SessionManagerStore::open_owned(&c).unwrap();
    drop(manager);
    let view = TransportRootsObservationV2::inspect(&[
        TransportRootV2::Desktop(c.clone()),
        TransportRootV2::ManagerFamilies(b.clone()),
        TransportRootV2::LocalData(a),
    ])
    .unwrap();
    view.verify_current().unwrap();
    assert!(session.lock().is_err());
    assert!(matches!(
        SessionManagerStore::open_family_owned(&b, &SessionId::new("session").unwrap()),
        Err(StoreError::Busy)
    ));
    assert!(matches!(
        SessionManagerStore::open_owned(&c),
        Err(StoreError::Busy)
    ));
    drop(view);
    session.lock().unwrap();
    SessionManagerStore::open_family_owned(&b, &SessionId::new("session").unwrap()).unwrap();
    SessionManagerStore::open_owned(&c).unwrap();
}

#[test]
fn root_set_later_busy_or_invalid_root_releases_every_prior_owner() {
    for busy in [false, true] {
        let temp = fixture();
        let a = temp.path().join("a-local");
        let b = temp.path().join("b-desktop");
        let session = local(&a);
        directory(&b);
        let owner = if busy {
            Some(SessionManagerStore::open_owned(&b).unwrap())
        } else {
            fs::write(b.join("unexpected"), b"retained").unwrap();
            None
        };
        assert!(
            TransportRootsObservationV2::inspect(&[
                TransportRootV2::LocalData(a),
                TransportRootV2::Desktop(b.clone()),
            ])
            .is_err()
        );
        session.lock().unwrap();
        if busy {
            assert!(matches!(
                SessionManagerStore::open_owned(&b),
                Err(StoreError::Busy)
            ));
        } else {
            assert_eq!(fs::read(b.join("unexpected")).unwrap(), b"retained");
        }
        drop(owner);
    }
}

#[test]
fn root_set_absence_is_read_only_and_closes_later_appearance() {
    let temp = fixture();
    let missing = temp.path().join("absent/deeper/workspace");
    let before = super::super::writer::migration::file_stamp(&temp.path().metadata().unwrap());
    let view = TransportRootsObservationV2::inspect(&[TransportRootV2::Desktop(missing)]).unwrap();
    view.verify_current().unwrap();
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    assert_eq!(
        super::super::writer::migration::file_stamp(&temp.path().metadata().unwrap()),
        before
    );
    directory(&temp.path().join("absent"));
    assert!(view.verify_current().is_err());
}

#[test]
fn root_set_rejects_duplicates_unsafe_modes_and_lexical_or_physical_aliases() {
    let temp = fixture();
    let path = temp.path().join("root");
    directory(&path);
    assert!(
        TransportRootsObservationV2::inspect(&[
            TransportRootV2::Desktop(path.clone()),
            TransportRootV2::LocalData(path.clone()),
        ])
        .is_err()
    );
    symlink(&path, temp.path().join("alias")).unwrap();
    for bad in [
        temp.path().join("alias"),
        PathBuf::from(format!("{}/./root", temp.path().display())),
        PathBuf::from(format!("{}//missing/deeper", temp.path().display())),
        PathBuf::from(format!("{}/missing/deeper/", temp.path().display())),
        PathBuf::from("relative"),
    ] {
        assert!(TransportRootsObservationV2::inspect(&[TransportRootV2::Desktop(bad)]).is_err());
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(TransportRootsObservationV2::inspect(&[TransportRootV2::Desktop(path)]).is_err());
    assert!(TransportRootsObservationV2::inspect(&[]).is_err());
}

#[test]
fn root_set_closes_earlier_namespace_change_during_later_construction() {
    let temp = fixture();
    let a = temp.path().join("a-local");
    let b = temp.path().join("b-desktop");
    let session = local(&a);
    directory(&b);
    let changed = a.join("sessions-v2/unexpected");
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_ROOTS_INSPECTED.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            directory(&changed);
            hit.set(true);
        }))
    });
    assert!(
        TransportRootsObservationV2::inspect(&[
            TransportRootV2::LocalData(a),
            TransportRootV2::Desktop(b)
        ])
        .is_err()
    );
    assert!(reached.get());
    session.lock().unwrap();
}

#[test]
fn root_set_closes_payload_drift_after_all_content_readers() {
    let temp = fixture();
    let a = temp.path().join("a-local");
    let session = local(&a);
    let view =
        TransportRootsObservationV2::inspect(&[TransportRootV2::LocalData(a.clone())]).unwrap();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_ROOTS_VERIFIED.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            let file = a.join("sessions-v2").join(LOCAL_ID).join("state.json");
            let mut bytes = fs::read(&file).unwrap();
            bytes.push(b' ');
            fs::write(file, bytes).unwrap();
            hit.set(true);
        }))
    });
    assert!(view.verify_current().is_err());
    assert!(reached.get());
    assert!(session.lock().is_err());
}

#[test]
fn root_set_retains_named_root_identity_and_rejects_replacement() {
    let temp = fixture();
    let root = temp.path().join("root");
    directory(&root);
    let view =
        TransportRootsObservationV2::inspect(&[TransportRootV2::Desktop(root.clone())]).unwrap();
    fs::rename(&root, temp.path().join("retained")).unwrap();
    directory(&root);
    assert!(view.verify_current().is_err());
    assert_eq!(
        fs::read_dir(temp.path().join("retained")).unwrap().count(),
        0
    );
}

#[test]
fn root_set_rejects_legacy_corruption_and_releases_current_writer() {
    let temp = fixture();
    let data = temp.path().join("data");
    let session = local(&data);
    directory(&data.join("sessions"));
    let path = data.join("sessions/legacy.jsonl");
    fs::write(&path, b"truncated{\n").unwrap();
    assert!(TransportRootsObservationV2::inspect(&[TransportRootV2::LocalData(data)]).is_err());
    assert_eq!(fs::read(path).unwrap(), b"truncated{\n");
    session.lock().unwrap();
}
