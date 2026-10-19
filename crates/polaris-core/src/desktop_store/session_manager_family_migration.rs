//! Read-only whole-family inventory; no index repair or manager recovery.
use super::super::super::{
    disk,
    writer::migration::{file_stamp, native_mount},
};
use super::super::migration::SessionManagerTransportInspectionV2;
use super::*;
use std::{collections::BTreeSet, fs::Metadata};

pub(in crate::desktop_store) struct FamilyTransportInspectionV2 {
    root: Directory,
    root_metadata: Metadata,
    mount: Arc<polaris_mcp::NativeMountIdentityV2>,
    names: Vec<String>,
    index_files: Vec<(&'static str, File, Metadata)>,
    sessions: BTreeMap<SessionId, SessionId>,
    families: BTreeMap<SessionId, FamilyPin>,
}
struct FamilyPin {
    directory: Directory,
    manager: SessionManagerTransportInspectionV2,
}
impl FamilyTransportInspectionV2 {
    pub(in crate::desktop_store) fn inspect(root: &Directory) -> StoreResult<Self> {
        let supplied = root.retained_file_m2a()?;
        let root_metadata = supplied.metadata()?;
        let mount = Arc::new(native_mount(&supplied)?);
        let root = Directory::open_owned(root.path())?.with_migration_mount(mount.clone())?;
        disk::same(&supplied, &root.retained_file_m2a()?)?;
        let mut index_files = Vec::new();
        let lock = match root.open(INDEX_LOCK, false, false) {
            Ok(file) => Some(file),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(lock) = lock {
            let metadata = lock.metadata()?;
            if metadata.len() != 0 {
                return Err(StoreError::RecoveryRequired);
            }
            match lock.try_lock() {
                Ok(()) => (),
                Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
                Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
            }
            index_files.push((INDEX_LOCK, lock, metadata));
        } else {
            // An empty unused root has no lock to acquire. Its retained FD and
            // exact empty namespace must remain unchanged through the outer gate.
            let view = Self {
                root,
                root_metadata,
                mount,
                names: Vec::new(),
                index_files,
                sessions: BTreeMap::new(),
                families: BTreeMap::new(),
            };
            view.verify_current()?;
            return Ok(view);
        }
        let index_file = match root.open(INDEX, false, false) {
            Ok(file) => Some(file),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let sessions = if let Some(file) = index_file {
            let metadata = file.metadata()?;
            index_files.push((INDEX, file, metadata));
            SessionManagerStore::read_family_index(&root)?
                .ok_or(StoreError::RecoveryRequired)?
                .sessions
        } else {
            BTreeMap::new()
        };
        let family_ids = sessions.values().cloned().collect::<BTreeSet<_>>();
        let mut names = index_files
            .iter()
            .map(|(name, _, _)| (*name).to_owned())
            .collect::<Vec<_>>();
        names.extend(family_ids.iter().map(|id| id.as_str().to_owned()));
        names.sort();
        if root.names_bounded_migration(names.len())? != names {
            return Err(StoreError::RecoveryRequired);
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_FAMILY_INDEX.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let mut families = BTreeMap::new();
        let mut observed = BTreeMap::new();
        // Sorted nonblocking acquisition retains every earlier owner until
        // either the whole observation succeeds or construction fails.
        for id in family_ids {
            let directory = root.child(id.as_str(), false)?;
            if directory.names_bounded_migration(1)? != [DIRECTORY] {
                return Err(StoreError::RecoveryRequired);
            }
            let manager = SessionManagerTransportInspectionV2::inspect(&directory)?;
            let state = manager
                .verified_state()?
                .ok_or(StoreError::RecoveryRequired)?;
            for session in state.sessions.keys() {
                if observed.insert(session.clone(), id.clone()).is_some() {
                    return Err(StoreError::RecoveryRequired);
                }
            }
            families.insert(id, FamilyPin { directory, manager });
        }
        if observed != sessions {
            return Err(StoreError::RecoveryRequired);
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_FAMILY_DECODE.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let view = Self {
            root,
            root_metadata,
            mount,
            names,
            index_files,
            sessions,
            families,
        };
        view.verify_current()?;
        Ok(view)
    }
    pub(in crate::desktop_store) fn verified_sessions(
        &self,
    ) -> StoreResult<&BTreeMap<SessionId, SessionId>> {
        self.verify_current()?;
        Ok(&self.sessions)
    }
    pub(in crate::desktop_store) fn verify_current(&self) -> StoreResult<()> {
        self.verify_root()?;
        for (name, file, metadata) in &self.index_files {
            let named = self.root.open(name, false, false)?;
            for current in [file, &named] {
                if native_mount(current)? != *self.mount
                    || file_stamp(&current.metadata()?) != file_stamp(metadata)
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
        }
        for (id, family) in &self.families {
            let named = self.root.child(id.as_str(), false)?;
            disk::same(
                &family.directory.retained_file_m2a()?,
                &named.retained_file_m2a()?,
            )?;
            if family.directory.names_bounded_migration(1)? != [DIRECTORY] {
                return Err(StoreError::RecoveryRequired);
            }
            family.manager.verify_current()?;
        }
        self.verify_root()
    }
    fn verify_root(&self) -> StoreResult<()> {
        self.root.verify()?;
        let file = self.root.retained_file_m2a()?;
        if native_mount(&file)? != *self.mount
            || file_stamp(&file.metadata()?) != file_stamp(&self.root_metadata)
            || self.root.names_bounded_migration(self.names.len())? != self.names
            || file_stamp(&file.metadata()?) != file_stamp(&self.root_metadata)
        {
            return Err(StoreError::RecoveryRequired);
        }
        self.root.verify()
    }
}
#[cfg(test)]
thread_local! {
    pub(super) static AFTER_FAMILY_INDEX: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_FAMILY_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    fn id(value: &str) -> SessionId {
        SessionId::new(value).unwrap()
    }
    fn setup() -> (tempfile::TempDir, Directory) {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = Directory::open_owned(temp.path()).unwrap();
        (temp, root)
    }
    fn family(root: &Directory, name: &str, members: &[&str]) {
        let directory = root.ensure_child(name).unwrap();
        let store = SessionManagerStore::open_owned(directory.path()).unwrap();
        let mut state = SessionManagerV1::new(id(members[0]));
        for member in &members[1..] {
            state.sessions.insert(
                id(member),
                SessionManagerV1::new(id(member))
                    .sessions
                    .remove(&id(member))
                    .unwrap(),
            );
        }
        store.initialize(&state).unwrap();
    }
    fn index(root: &Directory, entries: &[(&str, &str)]) {
        let _lock = SessionManagerStore::lock_family_index(root).unwrap();
        SessionManagerStore::write_family_index(
            root,
            &FamilyIndex {
                version: 1,
                sessions: entries
                    .iter()
                    .map(|(session, family)| (id(session), id(family)))
                    .collect(),
            },
        )
        .unwrap();
    }
    fn snapshot(root: &Path) -> Vec<(String, Vec<u8>, (u64, u64, u32, u64, i64, i64, i64, i64))> {
        fn visit(
            root: &Path,
            path: &Path,
            out: &mut Vec<(String, Vec<u8>, (u64, u64, u32, u64, i64, i64, i64, i64))>,
        ) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let m = path.symlink_metadata().unwrap();
                out.push((
                    path.strip_prefix(root).unwrap().to_str().unwrap().into(),
                    if m.is_file() {
                        fs::read(&path).unwrap()
                    } else {
                        vec![]
                    },
                    (
                        m.dev(),
                        m.ino(),
                        m.mode(),
                        m.nlink(),
                        m.mtime(),
                        m.mtime_nsec(),
                        m.ctime(),
                        m.ctime_nsec(),
                    ),
                ));
                if m.is_dir() {
                    visit(root, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        visit(root, root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
    #[test]
    fn migration_family_empty_lock_only_and_empty_index_are_read_only() {
        for mode in 0..3 {
            let (_temp, root) = setup();
            if mode == 1 {
                SessionManagerStore::lock_family_index(&root).unwrap();
            }
            if mode == 2 {
                index(&root, &[]);
            }
            let before = snapshot(root.path());
            let view = FamilyTransportInspectionV2::inspect(&root).unwrap();
            assert!(view.verified_sessions().unwrap().is_empty());
            assert_eq!(snapshot(root.path()), before);
            if mode != 0 {
                assert!(matches!(
                    SessionManagerStore::lock_family_index(&root),
                    Err(StoreError::Busy)
                ));
            }
        }
    }
    #[test]
    fn migration_family_holds_all_owners_and_exact_two_session_membership() {
        let (_temp, root) = setup();
        family(&root, "alpha", &["alpha", "sidechat"]);
        family(&root, "beta", &["beta"]);
        index(
            &root,
            &[("alpha", "alpha"), ("sidechat", "alpha"), ("beta", "beta")],
        );
        let before = snapshot(root.path());
        let view = FamilyTransportInspectionV2::inspect(&root).unwrap();
        assert_eq!(view.verified_sessions().unwrap().len(), 3);
        assert_eq!(snapshot(root.path()), before);
        assert!(matches!(
            SessionManagerStore::lock_family_index(&root),
            Err(StoreError::Busy)
        ));
        for name in ["alpha", "beta"] {
            assert!(matches!(
                SessionManagerStore::open_owned(&root.path().join(name)),
                Err(StoreError::Busy)
            ));
        }
        drop(view);
        SessionManagerStore::lock_family_index(&root).unwrap();
        for name in ["alpha", "beta"] {
            SessionManagerStore::open_owned(&root.path().join(name)).unwrap();
        }
    }
    #[test]
    fn migration_family_rejects_index_and_later_owner_busy_without_partial_retention() {
        for index_busy in [false, true] {
            let (_temp, root) = setup();
            family(&root, "alpha", &["alpha"]);
            family(&root, "beta", &["beta"]);
            index(&root, &[("alpha", "alpha"), ("beta", "beta")]);
            let index_owner = if index_busy {
                Some(SessionManagerStore::lock_family_index(&root).unwrap())
            } else {
                None
            };
            let beta_owner = if !index_busy {
                Some(SessionManagerStore::open_owned(&root.path().join("beta")).unwrap())
            } else {
                None
            };
            let before = snapshot(root.path());
            assert!(matches!(
                FamilyTransportInspectionV2::inspect(&root),
                Err(StoreError::Busy)
            ));
            assert_eq!(snapshot(root.path()), before);
            SessionManagerStore::open_owned(&root.path().join("alpha")).unwrap();
            drop(index_owner);
            drop(beta_owner);
            FamilyTransportInspectionV2::inspect(&root).unwrap();
        }
    }
    #[test]
    fn migration_family_rejects_missing_unindexed_duplicate_and_inexact_membership() {
        for mode in 0..6 {
            let (_temp, root) = setup();
            family(&root, "alpha", &["alpha", "sidechat"]);
            match mode {
                0 => index(&root, &[("alpha", "missing")]),
                1 => {
                    family(&root, "extra", &["extra"]);
                    index(&root, &[("alpha", "alpha"), ("sidechat", "alpha")]);
                }
                2 => {
                    family(&root, "beta", &["alpha"]);
                    index(
                        &root,
                        &[("alpha", "alpha"), ("sidechat", "alpha"), ("beta", "beta")],
                    );
                }
                3 => index(&root, &[("alpha", "alpha")]),
                4 => index(
                    &root,
                    &[
                        ("alpha", "alpha"),
                        ("sidechat", "alpha"),
                        ("ghost", "alpha"),
                    ],
                ),
                _ => {
                    SessionManagerStore::lock_family_index(&root).unwrap();
                }
            }
            let before = snapshot(root.path());
            assert!(FamilyTransportInspectionV2::inspect(&root).is_err());
            assert_eq!(snapshot(root.path()), before);
        }
    }
    #[test]
    fn migration_family_rejects_malformed_index_unknown_root_and_extra_family_child() {
        for mode in 0..4 {
            let (_temp, root) = setup();
            family(&root, "alpha", &["alpha"]);
            index(&root, &[("alpha", "alpha")]);
            match mode {
                0 => fs::write(root.path().join(INDEX), b"{bad").unwrap(),
                1 => fs::write(root.path().join("family-index.next"), b"unfinished").unwrap(),
                2 => fs::write(root.path().join("alpha").join("extra"), b"unknown").unwrap(),
                _ => fs::remove_file(root.path().join(INDEX_LOCK)).unwrap(),
            }
            let before = snapshot(root.path());
            assert!(FamilyTransportInspectionV2::inspect(&root).is_err());
            assert_eq!(snapshot(root.path()), before);
        }
    }
    #[test]
    fn migration_family_rejects_index_root_manager_and_namespace_drift() {
        for mode in 0..5 {
            let (_temp, root) = setup();
            family(&root, "alpha", &["alpha"]);
            index(&root, &[("alpha", "alpha")]);
            let view = FamilyTransportInspectionV2::inspect(&root).unwrap();
            match mode {
                0 => {
                    let bytes = fs::read(root.path().join(INDEX)).unwrap();
                    fs::write(root.path().join("replacement"), bytes).unwrap();
                    fs::set_permissions(
                        root.path().join("replacement"),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                    fs::rename(root.path().join("replacement"), root.path().join(INDEX)).unwrap();
                }
                1 => fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap(),
                2 => {
                    let path = root.path().join("alpha").join(DIRECTORY).join(JOURNAL);
                    fs::OpenOptions::new()
                        .append(true)
                        .open(path)
                        .unwrap()
                        .write_all(b"changed")
                        .unwrap();
                }
                3 => {
                    fs::create_dir(root.path().join("extra")).unwrap();
                }
                _ => {
                    fs::rename(root.path().join("alpha"), root.path().join("saved-alpha")).unwrap();
                }
            }
            assert!(view.verified_sessions().is_err());
        }
    }
    #[test]
    fn migration_family_closes_namespace_and_payload_changes_inside_constructor() {
        for after_decode in [false, true] {
            let (_temp, root) = setup();
            family(&root, "alpha", &["alpha"]);
            index(&root, &[("alpha", "alpha")]);
            let path = root.path().to_owned();
            let hook = Box::new(move || {
                if after_decode {
                    fs::OpenOptions::new()
                        .append(true)
                        .open(path.join("alpha").join(DIRECTORY).join(JOURNAL))
                        .unwrap()
                        .write_all(b"changed")
                        .unwrap();
                } else {
                    fs::write(path.join("unexpected"), b"added").unwrap();
                }
            }) as Box<dyn FnOnce()>;
            if after_decode {
                AFTER_FAMILY_DECODE.with(|slot| *slot.borrow_mut() = Some(hook));
            } else {
                AFTER_FAMILY_INDEX.with(|slot| *slot.borrow_mut() = Some(hook));
            }
            assert!(FamilyTransportInspectionV2::inspect(&root).is_err());
            AFTER_FAMILY_INDEX.with(|slot| assert!(slot.borrow().is_none()));
            AFTER_FAMILY_DECODE.with(|slot| assert!(slot.borrow().is_none()));
        }
    }
    #[test]
    fn migration_family_rejects_index_mount_before_decoding() {
        let (_temp, root) = setup();
        index(&root, &[]);
        let metadata = root.path().join(INDEX).metadata().unwrap();
        let reached = std::rc::Rc::new(Cell::new(false));
        let observed = reached.clone();
        AFTER_FAMILY_INDEX
            .with(|slot| *slot.borrow_mut() = Some(Box::new(move || observed.set(true))));
        super::super::super::super::writer::migration::REJECT_MOUNT_FILE
            .with(|slot| slot.set(Some((metadata.dev(), metadata.ino()))));
        let result = FamilyTransportInspectionV2::inspect(&root);
        super::super::super::super::writer::migration::REJECT_MOUNT_FILE
            .with(|slot| slot.set(None));
        AFTER_FAMILY_INDEX.with(|slot| slot.borrow_mut().take());
        assert!(matches!(result, Err(StoreError::RecoveryRequired)));
        assert!(!reached.get());
    }
}
