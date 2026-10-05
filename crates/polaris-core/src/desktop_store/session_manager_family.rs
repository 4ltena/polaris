//! Bounded family index reads and durable session-family membership.

use super::*;
use crate::session_manager::SessionId;
use std::collections::BTreeMap;

const INDEX: &str = "family-index.v1.json";
const INDEX_LOCK: &str = "family-index.lock";
const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FamilyIndex {
    version: u8,
    sessions: BTreeMap<SessionId, SessionId>,
}

impl SessionManagerStore {
    /// Reads an indexed family without creating, migrating or repairing files.
    /// The existing locks and bounded manager reader remain the authority.
    pub fn inspect_family_owned(
        root_path: &Path,
        current: &SessionId,
    ) -> StoreResult<Option<SessionManagerV1>> {
        let root = Directory::open_owned(root_path)?;
        let index_lock = root.open(INDEX_LOCK, false, false)?;
        match index_lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
        }
        let Some(index) = Self::read_family_index(&root)? else {
            return Ok(None);
        };
        let Some(family_id) = index.sessions.get(current) else {
            return Ok(None);
        };
        let family = root.child(family_id.as_str(), false)?;
        let directory = Arc::new(family.child(DIRECTORY, false)?);
        let lock = directory.open(OWNER_LOCK, false, false)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
        }
        let store = Self {
            directory,
            lock,
            legacy_verified: Cell::new(false),
            generation_verified: Cell::new(false),
        };
        let state = store
            .load()?
            .ok_or(StoreError::Corrupt("indexed family has no manager"))?;
        if !state.sessions.contains_key(current) {
            return Err(StoreError::Corrupt("session family index mismatch"));
        }
        root.verify()?;
        family.verify()?;
        store.verify_owner()?;
        super::super::disk::same(&index_lock, &root.open(INDEX_LOCK, false, false)?)?;
        Ok(Some(state))
    }

    /// Inspect both the family index and the child's directory before an
    /// unknown switch can be classified as having no published child.
    pub fn has_child_artifacts(root_path: &Path, child: &SessionId) -> StoreResult<bool> {
        let root = Directory::open_owned(root_path)?;
        let _index_lock = Self::lock_family_index(&root)?;
        let index = Self::read_family_index(&root)?;
        let directory = match std::fs::symlink_metadata(root.path().join(child.as_str())) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(StoreError::Corrupt(
                        "session family child is not a directory",
                    ));
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        Ok(directory || index.is_some_and(|index| index.sessions.contains_key(child)))
    }

    /// Finish only the manager/index publication for a verified durable v2
    /// child. The original conversation writer is never called here.
    pub fn repair_published_child(root_path: &Path, child: &SessionId) -> StoreResult<()> {
        let root = Directory::open_owned(root_path)?;
        let _index_lock = Self::lock_family_index(&root)?;
        let mut index = match Self::read_family_index(&root)? {
            Some(index) => index,
            None => Self::scan_family_index(&root)?,
        };
        if index
            .sessions
            .get(child)
            .is_some_and(|family| family != child)
        {
            return Err(StoreError::Corrupt("session switch child family mismatch"));
        }
        let child_root = root.ensure_child(child.as_str())?;
        let store = Self::open_owned(child_root.path())?;
        match store.load()? {
            Some(state) if state.sessions.len() == 1 && state.sessions.contains_key(child) => {}
            Some(_) => return Err(StoreError::Corrupt("session switch child manager mismatch")),
            None => store.initialize(&SessionManagerV1::new(child.clone()))?,
        }
        if index.sessions.insert(child.clone(), child.clone()) != Some(child.clone()) {
            Self::write_family_index(&root, &index)?;
        }
        Ok(())
    }

    /// Resolve a session through a durable family index. An unmapped session
    /// scans existing families once to repair an interrupted child publish or
    /// migrate a pre-index root; mapped continuations lock only their family.
    pub fn open_family_owned(
        root_path: &Path,
        current: &SessionId,
    ) -> StoreResult<(Self, SessionManagerV1)> {
        let root = Directory::open_owned(root_path)?;
        let _lock = Self::lock_family_index(&root)?;
        Self::open_family_locked(&root, current)
    }

    /// Keeps the source manager owner from the quiet check through the child
    /// writer's publish and the durable family-index update. The caller owns
    /// any project-wide effect/lease and supplies a fresh child identity.
    pub fn fork_family_owned<T>(
        root_path: &Path,
        source: &SessionId,
        child: &SessionId,
        publish: impl FnOnce(&SessionId, &SessionManagerV1) -> StoreResult<T>,
    ) -> StoreResult<T> {
        if source == child {
            return Err(StoreError::CasConflict("fork child identity"));
        }
        let root = Directory::open_owned(root_path)?;
        let _index_lock = Self::lock_family_index(&root)?;
        let (_source_store, state) = Self::open_family_locked(&root, source)?;
        state
            .ensure_fork_quiet(source)
            .map_err(|error| match error {
                crate::session_manager::AdmissionError::UnknownSession => StoreError::NotFound,
                crate::session_manager::AdmissionError::OutcomeUnknown => {
                    StoreError::RecoveryRequired
                }
                _ => StoreError::Busy,
            })?;
        let mut index = Self::read_family_index(&root)?
            .ok_or(StoreError::Corrupt("session family index missing"))?;
        if index.sessions.contains_key(child) {
            return Err(StoreError::CasConflict("fork child identity"));
        }
        let result = publish(child, &state)?;
        let child_root = root.ensure_child(child.as_str())?;
        let child_store = Self::open_owned(child_root.path())?;
        child_store.initialize(&SessionManagerV1::new(child.clone()))?;
        index.sessions.insert(child.clone(), child.clone());
        Self::write_family_index(&root, &index)?;
        Ok(result)
    }

    /// Publishes an independent new family without locking unrelated mapped
    /// families. A legacy root is indexed once before the child is written.
    pub fn create_family_owned<T>(
        root_path: &Path,
        child: &SessionId,
        publish: impl FnOnce(&SessionId) -> StoreResult<T>,
    ) -> StoreResult<T> {
        let root = Directory::open_owned(root_path)?;
        let _index_lock = Self::lock_family_index(&root)?;
        root.verify()?;
        let mut index = match Self::read_family_index(&root)? {
            Some(index) => index,
            None => {
                let index = Self::scan_family_index(&root)?;
                Self::write_family_index(&root, &index)?;
                index
            }
        };
        if index.sessions.contains_key(child) {
            return Err(StoreError::CasConflict("new child identity"));
        }
        let result = publish(child)?;
        let child_root = root.ensure_child(child.as_str())?;
        let child_store = Self::open_owned(child_root.path())?;
        child_store.initialize(&SessionManagerV1::new(child.clone()))?;
        index.sessions.insert(child.clone(), child.clone());
        Self::write_family_index(&root, &index)?;
        Ok(result)
    }

    fn lock_family_index(root: &Directory) -> StoreResult<std::fs::File> {
        let lock = open_or_create(root, INDEX_LOCK)?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => Err(StoreError::Io(error)),
        }
    }

    fn open_family_locked(
        root: &Directory,
        current: &SessionId,
    ) -> StoreResult<(Self, SessionManagerV1)> {
        root.verify()?;
        if let Some(index) = Self::read_family_index(root)?
            && let Some(family) = index.sessions.get(current)
        {
            let family = root.child(family.as_str(), false)?;
            let store = Self::open_owned(family.path())?;
            let state = store
                .load()?
                .ok_or(StoreError::Corrupt("indexed family has no manager"))?;
            if !state.sessions.contains_key(current) {
                return Err(StoreError::Corrupt("session family index mismatch"));
            }
            return Ok((store, state));
        }

        let mut index = Self::scan_family_index(root)?;
        if let Some(family_id) = index.sessions.get(current) {
            let family = root.child(family_id.as_str(), false)?;
            let store = Self::open_owned(family.path())?;
            let state = store
                .load()?
                .ok_or(StoreError::Corrupt("indexed family has no manager"))?;
            Self::write_family_index(root, &index)?;
            return Ok((store, state));
        }

        let family = root.ensure_child(current.as_str())?;
        let store = Self::open_owned(family.path())?;
        let state = SessionManagerV1::new(current.clone());
        store.initialize(&state)?;
        index.sessions.insert(current.clone(), current.clone());
        Self::write_family_index(root, &index)?;
        Ok((store, state))
    }

    fn scan_family_index(root: &Directory) -> StoreResult<FamilyIndex> {
        let mut index = FamilyIndex {
            version: 1,
            sessions: BTreeMap::new(),
        };
        for entry in std::fs::read_dir(root.path())? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(StoreError::Corrupt("session family symlink"));
            }
            if !file_type.is_dir() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::Corrupt("session family name"))?;
            let family_id =
                SessionId::new(name).map_err(|_| StoreError::Corrupt("session family name"))?;
            let family = root.child(family_id.as_str(), false)?;
            let store = Self::open_owned(family.path())?;
            let Some(state) = store.load()? else {
                continue;
            };
            for session_id in state.sessions.keys() {
                if index
                    .sessions
                    .insert(session_id.clone(), family_id.clone())
                    .is_some()
                {
                    return Err(StoreError::Corrupt("duplicate session family"));
                }
            }
            drop(store);
        }
        Ok(index)
    }

    fn read_family_index(root: &Directory) -> StoreResult<Option<FamilyIndex>> {
        let mut file = match root.open(INDEX, false, false) {
            Ok(file) => file,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let length = file.metadata()?.len();
        if length == 0 || length > MAX_INDEX_BYTES {
            return Err(StoreError::Corrupt("session family index size"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        (&mut file).take(length + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length {
            return Err(StoreError::Corrupt("session family index length"));
        }
        let index: FamilyIndex = serde_json::from_slice(&bytes)
            .map_err(|_| StoreError::Corrupt("session family index"))?;
        if index.version != 1 || serde_json::to_vec(&index)? != bytes {
            return Err(StoreError::Corrupt("session family index format"));
        }
        for (session, family) in &index.sessions {
            SessionId::new(session.as_str())
                .and_then(|_| SessionId::new(family.as_str()))
                .map_err(|_| StoreError::Corrupt("session family index id"))?;
        }
        Ok(Some(index))
    }

    fn write_family_index(root: &Directory, index: &FamilyIndex) -> StoreResult<()> {
        let bytes = serde_json::to_vec(index)?;
        if bytes.len() as u64 > MAX_INDEX_BYTES {
            return Err(StoreError::Capacity("session family index"));
        }
        let mut temporary = temporary_name();
        let mut file = loop {
            match root.open(&temporary, true, true) {
                Ok(file) => break file,
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    temporary = temporary_name();
                }
                Err(error) => return Err(error),
            }
        };
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        root.replace(&temporary, INDEX)?;
        root.sync()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session_manager::{
        AdmissionState, DestinationMode, SubmissionPayloadV1, SubmitOutcomeV1, SubmitRequestV1,
    };
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn interrupted_child_publication_repairs_only_manager_and_index() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let child = SessionId::new("recovered-child").unwrap();
        assert!(!SessionManagerStore::has_child_artifacts(&path, &child).unwrap());
        let partial = path.join(child.as_str());
        fs::create_dir(&partial).unwrap();
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(SessionManagerStore::has_child_artifacts(&path, &child).unwrap());
        SessionManagerStore::repair_published_child(&path, &child).unwrap();
        SessionManagerStore::repair_published_child(&path, &child).unwrap();
        let (store, state) = SessionManagerStore::open_family_owned(&path, &child).unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions.contains_key(&child));
        drop(store);
        assert!(
            fs::read(path.join(INDEX))
                .unwrap()
                .starts_with(b"{\"version\":1")
        );
    }

    #[test]
    fn conflicting_recovered_child_manager_stays_unresolved() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let child = SessionId::new("expected-child").unwrap();
        let other = SessionId::new("different-child").unwrap();
        let partial = path.join(child.as_str());
        fs::create_dir(&partial).unwrap();
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o700)).unwrap();
        let store = SessionManagerStore::open_owned(&partial).unwrap();
        store.initialize(&SessionManagerV1::new(other)).unwrap();
        drop(store);
        assert!(SessionManagerStore::repair_published_child(&path, &child).is_err());
        assert!(!path.join(INDEX).exists());
    }

    #[test]
    fn indexed_continuation_avoids_unrelated_family_lock() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let current = SessionId::new("current").unwrap();
        let (store, state) = SessionManagerStore::open_family_owned(&path, &current).unwrap();
        assert!(state.sessions.contains_key(&current));
        drop(store);
        let other = path.join("other");
        fs::create_dir(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
        let other_store = SessionManagerStore::open_owned(&other).unwrap();
        other_store
            .initialize(&SessionManagerV1::new(SessionId::new("other").unwrap()))
            .unwrap();
        let (store, state) = SessionManagerStore::open_family_owned(&path, &current).unwrap();
        assert!(state.sessions.contains_key(&current));
        drop(store);
        drop(other_store);
    }

    #[test]
    fn unindexed_sidechat_is_found_after_destination_publish() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let current = SessionId::new("current").unwrap();
        let (store, mut state) = SessionManagerStore::open_family_owned(&path, &current).unwrap();
        let request = SubmitRequestV1 {
            parent_session: current,
            payload: SubmissionPayloadV1 {
                text: "different task".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::New,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted { admission, .. } =
            state.submit(request, state.revision).unwrap();
        store
            .persist_submission(
                0,
                &state,
                &BTreeMap::from([(SessionId::new("current").unwrap(), 0)]),
            )
            .unwrap();
        let before = state.revision;
        let decided = state
            .decide(
                &admission.id,
                state.revision,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        store.persist(before, &state).unwrap();
        let before = state.revision;
        let prepared = state
            .prepare_destination(&admission.id, state.revision, decided.revision)
            .unwrap();
        let AdmissionState::DestinationReady { session_id, .. } = prepared.state else {
            panic!("sidechat was not prepared");
        };
        store.persist(before, &state).unwrap();
        drop(store);
        let (sidechat_store, recovered) =
            SessionManagerStore::open_family_owned(&path, &session_id).unwrap();
        assert!(recovered.sessions.contains_key(&session_id));
        drop(sidechat_store);
        assert!(
            fs::read(path.join(INDEX))
                .unwrap()
                .windows(session_id.as_str().len())
                .any(|window| window == session_id.as_str().as_bytes())
        );
    }

    #[test]
    fn fork_holds_source_and_index_until_child_mapping_is_durable() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let source = SessionId::new("source").unwrap();
        let child = SessionId::new("child").unwrap();
        let (store, _) = SessionManagerStore::open_family_owned(&path, &source).unwrap();
        drop(store);

        SessionManagerStore::fork_family_owned(&path, &source, &child, |target, state| {
            assert_eq!(target, &child);
            assert_eq!(state.revision, 0);
            assert!(matches!(
                SessionManagerStore::open_family_owned(&path, &source),
                Err(StoreError::Busy)
            ));
            Ok(())
        })
        .unwrap();
        let (child_store, child_state) =
            SessionManagerStore::open_family_owned(&path, &child).unwrap();
        assert_eq!(child_state.sessions.len(), 1);
        assert!(child_state.sessions.contains_key(&child));
        drop(child_store);
        let (source_store, source_state) =
            SessionManagerStore::open_family_owned(&path, &source).unwrap();
        assert_eq!(source_state.revision, 0);
        drop(source_store);
    }

    #[test]
    fn fork_refuses_pending_input_before_child_publish() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let source = SessionId::new("source").unwrap();
        let child = SessionId::new("child").unwrap();
        let (store, mut state) = SessionManagerStore::open_family_owned(&path, &source).unwrap();
        let request = SubmitRequestV1 {
            parent_session: source.clone(),
            payload: SubmissionPayloadV1 {
                text: "pending".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        state.submit(request, state.revision).unwrap();
        store
            .persist_submission(0, &state, &BTreeMap::from([(source.clone(), 0)]))
            .unwrap();
        drop(store);

        let mut published = false;
        let result = SessionManagerStore::fork_family_owned(&path, &source, &child, |_, _| {
            published = true;
            Ok(())
        });
        assert!(matches!(result, Err(StoreError::Busy)));
        assert!(!published);
        assert!(!path.join(child.as_str()).exists());
    }

    #[test]
    fn new_family_does_not_wait_for_an_unrelated_indexed_owner() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let previous = SessionId::new("previous").unwrap();
        let child = SessionId::new("child").unwrap();
        let (store, _) = SessionManagerStore::open_family_owned(&path, &previous).unwrap();
        let mut published = false;
        SessionManagerStore::create_family_owned(&path, &child, |_| {
            published = true;
            Ok(())
        })
        .unwrap();
        assert!(published);
        drop(store);
        let (child_store, child_state) =
            SessionManagerStore::open_family_owned(&path, &child).unwrap();
        assert!(child_state.sessions.contains_key(&child));
        drop(child_store);
    }

    #[test]
    fn failed_child_publish_keeps_source_and_does_not_map_child() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(root.path()).unwrap();
        let source = SessionId::new("source").unwrap();
        let child = SessionId::new("child").unwrap();
        let (store, _) = SessionManagerStore::open_family_owned(&path, &source).unwrap();
        drop(store);

        let result = SessionManagerStore::fork_family_owned(&path, &source, &child, |target, _| {
            fs::write(
                path.join(format!("{}.partial", target.as_str())),
                b"published",
            )?;
            Err::<(), _>(StoreError::Io(std::io::Error::other("interrupted")))
        });
        assert!(matches!(result, Err(StoreError::Io(_))));
        assert!(!path.join(child.as_str()).exists());
        let index = fs::read_to_string(path.join(INDEX)).unwrap();
        assert!(!index.contains(child.as_str()));
        let (store, state) = SessionManagerStore::open_family_owned(&path, &source).unwrap();
        assert_eq!(state.revision, 0);
        drop(store);
    }
}
