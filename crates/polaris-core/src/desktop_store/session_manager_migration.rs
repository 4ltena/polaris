//! Private no-create/no-repair observations for the MCP migration owner.
use super::super::{
    disk,
    writer::migration::{file_stamp, native_mount},
};
use super::*;
use std::{collections::BTreeSet, fs::Metadata};

pub(in crate::desktop_store) struct SessionManagerTransportInspectionV2 {
    store: SessionManagerStore,
    state: Option<SessionManagerV1>,
    mount: Arc<polaris_mcp::NativeMountIdentityV2>,
    directories: Vec<DirectoryPin>,
    files: Vec<FilePin>,
}
struct DirectoryPin {
    directory: Arc<Directory>,
    metadata: Metadata,
    names: Option<Vec<String>>,
}
struct FilePin {
    directory: Arc<Directory>,
    name: String,
    file: File,
    metadata: Metadata,
}

impl SessionManagerTransportInspectionV2 {
    pub(in crate::desktop_store) fn inspect(root: &Directory) -> StoreResult<Self> {
        let mut remaining = u64::MAX;
        Self::inspect_bounded(root, &mut remaining)
    }
    pub(in crate::desktop_store) fn inspect_bounded(
        root: &Directory,
        remaining: &mut u64,
    ) -> StoreResult<Self> {
        let supplied = root.retained_file_m2a()?;
        let mount = Arc::new(native_mount(&supplied)?);
        let root =
            Arc::new(Directory::open_owned(root.path())?.with_migration_mount(mount.clone())?);
        disk::same(&supplied, &root.retained_file_m2a()?)?;
        let root_metadata = supplied.metadata()?;
        let directory = Arc::new(root.child(DIRECTORY, false)?);
        let directory_metadata = directory.retained_file_m2a()?.metadata()?;
        if native_mount(&directory.retained_file_m2a()?)? != *mount {
            return Err(StoreError::RecoveryRequired);
        }
        let lock = directory.open(OWNER_LOCK, false, false)?;
        if lock.metadata()?.len() != 0 || native_mount(&lock)? != *mount {
            return Err(StoreError::RecoveryRequired);
        }
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
        }
        let store = SessionManagerStore {
            directory: directory.clone(),
            lock,
            legacy_verified: Cell::new(false),
            generation_verified: Cell::new(false),
        };
        // Charge the existing namespace before layout discovery decodes any
        // revision. Retain these files and reject changes during discovery.
        let mut budget_pins = Vec::new();
        budget_files(&directory, "", remaining, &mut budget_pins)?;
        let layout = store.migration_layout()?;
        for (_, file, metadata) in &budget_pins {
            if file_stamp(&file.metadata()?) != file_stamp(metadata) {
                return Err(StoreError::RecoveryRequired);
            }
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_MANAGER_LAYOUT.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let mut named = BTreeMap::from([(String::new(), directory.clone())]);
        if layout.keys().any(|name| name.starts_with("generations/1/")) {
            let generations = Arc::new(directory.child("generations", false)?);
            let active = Arc::new(generations.child("1", false)?);
            named.insert("generations".into(), generations);
            named.insert("generations/1".into(), active);
        }
        let mut directories = vec![DirectoryPin {
            directory: root,
            metadata: root_metadata,
            names: None,
        }];
        for (relative, current) in &named {
            let mut names = BTreeSet::new();
            for path in layout.keys().chain(named.keys()) {
                if path.is_empty() {
                    continue;
                }
                let remainder = if relative.is_empty() {
                    Some(path.as_str())
                } else {
                    path.strip_prefix(&format!("{relative}/"))
                };
                if let Some(remainder) = remainder {
                    names.insert(remainder.split('/').next().unwrap().to_owned());
                }
            }
            let names = names.into_iter().collect::<Vec<_>>();
            if current.names_bounded_migration(names.len())? != names {
                return Err(StoreError::RecoveryRequired);
            }
            let metadata = if relative.is_empty() {
                directory_metadata.clone()
            } else {
                current.retained_file_m2a()?.metadata()?
            };
            directories.push(DirectoryPin {
                directory: current.clone(),
                metadata,
                names: Some(names),
            });
        }
        let mut files = Vec::new();
        if layout.len() != budget_pins.len() {
            return Err(StoreError::RecoveryRequired);
        }
        for (path, maximum) in &layout {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let directory = named
                .get(parent)
                .ok_or(StoreError::RecoveryRequired)?
                .clone();
            let file = directory.open(name, false, false)?;
            let metadata = file.metadata()?;
            let (_, budget_file, budget_metadata) = budget_pins
                .iter()
                .find(|(name, _, _)| name == path)
                .ok_or(StoreError::RecoveryRequired)?;
            disk::same(&file, budget_file)?;
            if file_stamp(&metadata) != file_stamp(budget_metadata) {
                return Err(StoreError::RecoveryRequired);
            }
            if metadata.len() > *maximum || native_mount(&file)? != *mount {
                return Err(StoreError::RecoveryRequired);
            }
            if name == OWNER_LOCK {
                disk::same(&store.lock, &file)?;
            }
            files.push(FilePin {
                directory,
                name: name.to_owned(),
                file,
                metadata,
            });
        }
        // Re-read every referenced revision after all FDs are pinned. Fresh
        // layout verification never uses the ordinary load's verification cache.
        if store.migration_layout()? != layout {
            return Err(StoreError::RecoveryRequired);
        }
        let state = store.load()?;
        if let Some(state) = &state {
            let source = state
                .sessions
                .keys()
                .next()
                .ok_or(StoreError::RecoveryRequired)?;
            state
                .ensure_fork_quiet(source)
                .map_err(|_| StoreError::RecoveryRequired)?;
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_MANAGER_DECODE.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let view = Self {
            store,
            state,
            mount,
            directories,
            files,
        };
        view.verify_current()?;
        Ok(view)
    }
    pub(in crate::desktop_store) fn verified_state(
        &self,
    ) -> StoreResult<Option<&SessionManagerV1>> {
        self.verify_current()?;
        Ok(self.state.as_ref())
    }
    pub(in crate::desktop_store) fn verify_current(&self) -> StoreResult<()> {
        self.store.verify_owner()?;
        self.verify_directories()?;
        for pin in &self.files {
            let named = pin.directory.open(&pin.name, false, false)?;
            for file in [&pin.file, &named] {
                if native_mount(file)? != *self.mount
                    || file_stamp(&file.metadata()?) != file_stamp(&pin.metadata)
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
        }
        self.verify_directories()
    }
    fn verify_directories(&self) -> StoreResult<()> {
        for pin in &self.directories {
            pin.directory.verify()?;
            let file = pin.directory.retained_file_m2a()?;
            if native_mount(&file)? != *self.mount
                || file_stamp(&file.metadata()?) != file_stamp(&pin.metadata)
            {
                return Err(StoreError::RecoveryRequired);
            }
            if let Some(names) = &pin.names {
                if pin.directory.names_bounded_migration(names.len())? != *names {
                    return Err(StoreError::RecoveryRequired);
                }
            }
            if file_stamp(&file.metadata()?) != file_stamp(&pin.metadata) {
                return Err(StoreError::RecoveryRequired);
            }
            pin.directory.verify()?;
        }
        Ok(())
    }
}

fn budget_files(
    directory: &Directory,
    relative: &str,
    remaining: &mut u64,
    pins: &mut Vec<(String, File, Metadata)>,
) -> StoreResult<()> {
    for name in directory.names_bounded_migration(65_536)? {
        if (relative.is_empty() && name == "generations")
            || (relative == "generations" && name == "1")
        {
            let child = directory.child(&name, false)?;
            let next = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            budget_files(&child, &next, remaining, pins)?;
        } else {
            if pins.len() >= 65_536 {
                return Err(StoreError::Capacity("manager inspected files"));
            }
            let file = directory.open(&name, false, false)?;
            let metadata = file.metadata()?;
            *remaining = remaining
                .checked_sub(metadata.len())
                .ok_or(StoreError::Capacity("desktop root inspected bytes"))?;
            let path = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            pins.push((path, file, metadata));
        }
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    pub(super) static AFTER_MANAGER_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_MANAGER_LAYOUT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::{
        DestinationMode, SessionId, SubmissionPayloadV1, SubmitOutcomeV1,
    };
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    fn setup(initialize: bool) -> (tempfile::TempDir, Arc<Directory>) {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = Arc::new(Directory::open_owned(root.path()).unwrap());
        let store = SessionManagerStore::from_root(directory.clone()).unwrap();
        if initialize {
            store
                .initialize(&SessionManagerV1::new(SessionId::new("current").unwrap()))
                .unwrap();
        }
        drop(store);
        (root, directory)
    }
    fn snapshot(path: &Path) -> Vec<(String, Vec<u8>, (u64, u64, u32, u64, i64, i64, i64, i64))> {
        fn visit(
            base: &Path,
            path: &Path,
            entries: &mut Vec<(String, Vec<u8>, (u64, u64, u32, u64, i64, i64, i64, i64))>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let m = path.symlink_metadata().unwrap();
                entries.push((
                    path.strip_prefix(base).unwrap().to_str().unwrap().into(),
                    if m.is_file() {
                        std::fs::read(&path).unwrap()
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
                    visit(base, &path, entries);
                }
            }
        }
        let mut entries = Vec::new();
        visit(path, path, &mut entries);
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
    #[test]
    fn migration_manager_read_only_legacy_empty_and_lock_retention() {
        for initialize in [false, true] {
            let (_temp, root) = setup(initialize);
            let before = snapshot(root.path());
            let view = SessionManagerTransportInspectionV2::inspect(&root).unwrap();
            assert_eq!(view.verified_state().unwrap().is_some(), initialize);
            assert_eq!(snapshot(root.path()), before);
            assert!(matches!(
                SessionManagerStore::from_root(root.clone()),
                Err(StoreError::Busy)
            ));
            drop(view);
            SessionManagerStore::from_root(root).unwrap();
        }
    }
    #[test]
    fn migration_manager_rejects_active_owner_without_repair() {
        let (_temp, root) = setup(true);
        let _owner = SessionManagerStore::from_root(root.clone()).unwrap();
        let before = snapshot(root.path());
        assert!(matches!(
            SessionManagerTransportInspectionV2::inspect(&root),
            Err(StoreError::Busy)
        ));
        assert_eq!(snapshot(root.path()), before);
    }
    #[test]
    fn migration_manager_rejects_torn_journal_without_repair() {
        for initialize in [false, true] {
            let (_temp, root) = setup(initialize);
            let path = root.path().join(DIRECTORY).join(JOURNAL);
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .open(path)
                .unwrap()
                .write_all(b"torn")
                .unwrap();
            let before = snapshot(root.path());
            assert!(SessionManagerTransportInspectionV2::inspect(&root).is_err());
            assert_eq!(snapshot(root.path()), before);
        }
    }
    #[test]
    fn migration_manager_rejects_pending_and_unknown_without_reclassification() {
        for unknown in [false, true] {
            let (_temp, root) = setup(true);
            let store = SessionManagerStore::from_root(root.clone()).unwrap();
            let mut state = store.load().unwrap().unwrap();
            let request = SubmitRequestV1 {
                parent_session: SessionId::new("current").unwrap(),
                payload: SubmissionPayloadV1 {
                    text: "pending".into(),
                    attachment_ids: vec![],
                    route: DestinationMode::Current,
                    expected_session_revision: 0,
                    expected_goal_revision: 0,
                },
                admission_id: None,
            };
            let SubmitOutcomeV1::Accepted { admission, .. } =
                state.submit(request, state.revision).unwrap();
            if unknown {
                state.queue.retain(|id| id != &admission.id);
                state.admissions.get_mut(&admission.id).unwrap().state = AdmissionState::Terminal {
                    session_id: None,
                    run_id: None,
                    status: TerminalStatus::OutcomeUnknown,
                };
            }
            store.append_commit(&state).unwrap();
            drop(store);
            let before = snapshot(root.path());
            assert!(matches!(
                SessionManagerTransportInspectionV2::inspect(&root),
                Err(StoreError::RecoveryRequired)
            ));
            assert_eq!(snapshot(root.path()), before);
        }
    }
    #[test]
    fn migration_manager_rejects_orphan_and_namespace_changes() {
        for mutation in 0..3 {
            let (_temp, root) = setup(true);
            let view = SessionManagerTransportInspectionV2::inspect(&root).unwrap();
            let path = root.path().join(DIRECTORY);
            match mutation {
                0 => std::fs::write(path.join("state-orphan.json"), b"orphan").unwrap(),
                1 => {
                    let name = state_name(0).unwrap();
                    let bytes = std::fs::read(path.join(&name)).unwrap();
                    std::fs::write(path.join("replacement"), bytes).unwrap();
                    std::fs::set_permissions(
                        path.join("replacement"),
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                    std::fs::rename(path.join("replacement"), path.join(name)).unwrap();
                }
                _ => {
                    std::fs::rename(&path, root.path().join("saved-manager")).unwrap();
                    std::fs::create_dir(&path).unwrap();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
            }
            assert!(view.verified_state().is_err());
            drop(view);
            let before = snapshot(root.path());
            let next = SessionManagerTransportInspectionV2::inspect(&root);
            if mutation == 1 {
                // A fresh observation may accept a valid same-byte replacement;
                // the stale held observation above must expire.
                assert!(next.is_ok());
            } else {
                assert!(next.is_err());
            }
            assert_eq!(snapshot(root.path()), before);
        }
    }
    #[test]
    fn migration_manager_closes_changes_after_decode() {
        let (_temp, root) = setup(true);
        let path = root.path().join(DIRECTORY).join(JOURNAL);
        AFTER_MANAGER_DECODE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(b"changed")
                    .unwrap();
            }))
        });
        assert!(SessionManagerTransportInspectionV2::inspect(&root).is_err());
        AFTER_MANAGER_DECODE.with(|slot| assert!(slot.borrow().is_none()));
    }
    #[test]
    fn migration_manager_mount_rejection_precedes_semantic_layout() {
        let (_temp, root) = setup(true);
        let metadata = root
            .path()
            .join(DIRECTORY)
            .join(JOURNAL)
            .metadata()
            .unwrap();
        let reached = std::rc::Rc::new(Cell::new(false));
        let observed = reached.clone();
        AFTER_MANAGER_LAYOUT
            .with(|slot| *slot.borrow_mut() = Some(Box::new(move || observed.set(true))));
        super::super::super::writer::migration::REJECT_MOUNT_FILE
            .with(|slot| slot.set(Some((metadata.dev(), metadata.ino()))));
        let result = SessionManagerTransportInspectionV2::inspect(&root);
        super::super::super::writer::migration::REJECT_MOUNT_FILE.with(|slot| slot.set(None));
        AFTER_MANAGER_LAYOUT.with(|slot| slot.borrow_mut().take());
        assert!(matches!(result, Err(StoreError::RecoveryRequired)));
        assert!(!reached.get());
    }
    #[test]
    fn migration_manager_reads_more_than_single_session_entry_limit() {
        let (_temp, root) = setup(true);
        let store = SessionManagerStore::from_root(root.clone()).unwrap();
        let mut state = store.load().unwrap().unwrap();
        for revision in 1..=1024 {
            state.revision = revision;
            store.append_commit(&state).unwrap();
        }
        drop(store);
        let before = snapshot(root.path());
        let view = SessionManagerTransportInspectionV2::inspect(&root).unwrap();
        assert_eq!(view.verified_state().unwrap().unwrap().revision, 1024);
        assert_eq!(snapshot(root.path()), before);
    }
    #[test]
    fn migration_manager_budget_aggregates_valid_files_before_decode() {
        let (_temp, root) = setup(true);
        let before = snapshot(root.path());
        let mut pins = Vec::new();
        let mut unlimited = u64::MAX;
        budget_files(
            &root.child(DIRECTORY, false).unwrap(),
            "",
            &mut unlimited,
            &mut pins,
        )
        .unwrap();
        let total = u64::MAX - unlimited;
        assert!(total > 0);
        assert!(
            pins.iter()
                .filter(|(_, _, metadata)| metadata.len() > 0)
                .count()
                >= 2
        );
        assert!(pins.iter().all(|(_, _, metadata)| metadata.len() < total));
        drop(pins);
        let mut budget = total - 1;
        assert!(matches!(
            SessionManagerTransportInspectionV2::inspect_bounded(&root, &mut budget),
            Err(StoreError::Capacity("desktop root inspected bytes"))
        ));
        assert_eq!(snapshot(root.path()), before);
        let mut exact = total;
        SessionManagerTransportInspectionV2::inspect_bounded(&root, &mut exact).unwrap();
        assert_eq!(exact, 0);
    }
}
