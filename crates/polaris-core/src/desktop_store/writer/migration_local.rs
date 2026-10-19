//! Read-only local conversation migration inspection through pinned directories.
use super::*;
use crate::conversation_state::{self as conversation, ConversationSnapshot, ConversationStateV2};
use std::{
    collections::BTreeMap,
    fs::Metadata,
    io::{Read, Seek, SeekFrom},
};

// Complete observation fails closed above these existing inventory/pin resource limits.
const MAX_SNAPSHOT_ENTRIES: usize = 65_536;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

fn verify_parent_identity(
    file: &File,
    expected: polaris_tools::entry_kernel::EntryAnchorV2,
    mount: &polaris_mcp::NativeMountIdentityV2,
) -> StoreResult<()> {
    // The ordinary data root also holds settings/cache and, at .polaris,
    // separately pinned coordinator intent/staging siblings. Both the whole
    // container and each session bind its identity/protection instead of the
    // sibling-sensitive stamp. sessions-v2 and everything below stay exact.
    let current = polaris_tools::entry_kernel::EntryAnchorV2::of(&file.metadata()?);
    if (current.device, current.inode, current.uid, current.mode)
        != (expected.device, expected.inode, expected.uid, expected.mode)
        || native_mount(file)? != *mount
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(())
}

// A complete local container observation, still not a stop or activation proof.
const MAX_LOCAL_SESSIONS: usize = 65_536;
pub(in crate::desktop_store) struct LocalSessionsTransportInspectionV2 {
    parent: disk::OwnerControlledContainer,
    parent_anchor: polaris_tools::entry_kernel::EntryAnchorV2,
    mount: Arc<polaris_mcp::NativeMountIdentityV2>,
    container: Option<(disk::OwnerControlledContainer, Metadata)>,
    names: Vec<String>,
    sessions: BTreeMap<String, LocalSessionTransportInspectionV2>,
}
impl LocalSessionsTransportInspectionV2 {
    pub(in crate::desktop_store) fn inspect(
        parent: &disk::OwnerControlledContainer,
    ) -> StoreResult<Self> {
        let supplied = parent.retained_file_m2a()?;
        let parent_anchor = polaris_tools::entry_kernel::EntryAnchorV2::of(&supplied.metadata()?);
        let mount = Arc::new(native_mount(&supplied)?);
        let parent = disk::OwnerControlledContainer::open_migration(parent.path())?;
        let held = parent.retained_file_m2a()?;
        disk::same(&supplied, &held)?;
        if native_mount(&held)? != *mount {
            return Err(StoreError::RecoveryRequired);
        }
        let container = parent
            .local_container_migration()?
            .map(|directory| {
                let metadata = directory.retained_file_m2a()?.metadata()?;
                Ok::<_, StoreError>((directory, metadata))
            })
            .transpose()?;
        let names = match &container {
            Some((directory, _)) => directory.names_bounded_migration(MAX_LOCAL_SESSIONS)?,
            None => Vec::new(),
        };
        if names.iter().any(|id| !conversation::valid_session_id(id)) {
            return Err(StoreError::RecoveryRequired);
        }
        #[cfg(test)]
        AFTER_LOCAL_CONTAINER_NAMES.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let mut sessions = BTreeMap::new();
        // Sorted nonblocking acquisition keeps every previous existing writer lock.
        // A later failure drops all owners instead of publishing partial observation.
        for id in &names {
            let session = LocalSessionTransportInspectionV2::inspect(&parent, id)?;
            sessions.insert(id.clone(), session);
        }
        #[cfg(test)]
        AFTER_LOCAL_CONTAINER_DECODE.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let view = Self {
            parent,
            parent_anchor,
            mount,
            container,
            names,
            sessions,
        };
        view.verify_current()?;
        Ok(view)
    }
    pub(in crate::desktop_store) fn verified_sessions(
        &self,
    ) -> StoreResult<&BTreeMap<String, LocalSessionTransportInspectionV2>> {
        self.verify_current()?;
        Ok(&self.sessions)
    }
    pub(in crate::desktop_store) fn verify_current(&self) -> StoreResult<()> {
        self.verify_coordinates()?;
        self.verify_names()?;
        for session in self.sessions.values() {
            session.verify_current()?;
        }
        self.verify_coordinates()?;
        self.verify_names()?;
        #[cfg(test)]
        AFTER_LOCAL_CONTAINER_FINAL_NAMES.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        self.verify_coordinates()
    }
    fn verify_names(&self) -> StoreResult<()> {
        match &self.container {
            Some((directory, _))
                if directory.names_bounded_migration(self.names.len())? == self.names =>
            {
                Ok(())
            }
            None if self.names.is_empty() && self.sessions.is_empty() => Ok(()),
            _ => Err(StoreError::RecoveryRequired),
        }
    }
    fn verify_coordinates(&self) -> StoreResult<()> {
        self.parent.verify()?;
        let parent = self.parent.retained_file_m2a()?;
        verify_parent_identity(&parent, self.parent_anchor, &self.mount)?;
        let named = self.parent.local_container_migration()?;
        match (&self.container, named) {
            (None, None) => (),
            (Some((held, metadata)), Some(named)) => {
                let held = held.retained_file_m2a()?;
                let named = named.retained_file_m2a()?;
                disk::same(&held, &named)?;
                for file in [&held, &named] {
                    if native_mount(file)? != *self.mount
                        || file_stamp(&file.metadata()?) != file_stamp(metadata)
                    {
                        return Err(StoreError::RecoveryRequired);
                    }
                }
            }
            _ => return Err(StoreError::RecoveryRequired),
        }
        self.parent.verify()?;
        verify_parent_identity(
            &self.parent.retained_file_m2a()?,
            self.parent_anchor,
            &self.mount,
        )
    }
}

pub(in crate::desktop_store) struct LocalSessionTransportInspectionV2 {
    parent: disk::OwnerControlledContainer,
    container: File,
    container_metadata: Metadata,
    session: String,
    directory: disk::Directory,
    snapshots: disk::Directory,
    directories: Vec<Metadata>,
    names: Vec<String>,
    snapshot_names: Vec<String>,
    mount: Arc<polaris_mcp::NativeMountIdentityV2>,
    files: Vec<LocalFilePin>,
    current: ConversationSnapshot,
}
struct LocalFilePin {
    snapshot: bool,
    name: String,
    file: File,
    metadata: Metadata,
}
impl LocalSessionTransportInspectionV2 {
    pub(in crate::desktop_store) fn inspect(
        parent: &disk::OwnerControlledContainer,
        session: &str,
    ) -> StoreResult<Self> {
        let supplied = parent.retained_file_m2a()?;
        let mount = Arc::new(native_mount(&supplied)?);
        let parent = disk::OwnerControlledContainer::open_migration(parent.path())?;
        if native_mount(&parent.retained_file_m2a()?)? != *mount {
            return Err(StoreError::RecoveryRequired);
        }
        disk::same(&supplied, &parent.retained_file_m2a()?)?;
        let parent_metadata = parent.retained_file_m2a()?.metadata()?;
        let (container, directory) = parent.existing_local_session_migration(session)?;
        let container_metadata = container.metadata()?;
        let directory_metadata = directory.retained_file_m2a()?.metadata()?;
        let names = directory.names_bounded_migration(7)?;
        for name in ["raw.jsonl", "snapshots", "state.json", "writer.lock"] {
            if !names.iter().any(|candidate| candidate == name) {
                return Err(StoreError::RecoveryRequired);
            }
        }
        if names.iter().any(|name| {
            ![
                "raw.jsonl",
                "snapshots",
                "state.json",
                "writer.lock",
                "rewind-branch-v1.json",
                "rewind-boundaries-v1.json",
                "workspace-head-binding-v1.json",
            ]
            .contains(&name.as_str())
        }) {
            return Err(StoreError::RecoveryRequired);
        }
        let snapshots = directory.child("snapshots", false)?;
        let snapshots_metadata = snapshots.retained_file_m2a()?.metadata()?;
        let lock = directory.open("writer.lock", false, false)?;
        if lock.metadata()?.len() != 0 {
            return Err(StoreError::RecoveryRequired);
        }
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
        }
        let mut files = vec![LocalFilePin {
            snapshot: false,
            name: "writer.lock".into(),
            metadata: lock.metadata()?,
            file: lock,
        }];
        for name in names
            .iter()
            .filter(|name| *name != "snapshots" && *name != "writer.lock")
        {
            let file = directory.open(name, false, false)?;
            files.push(LocalFilePin {
                snapshot: false,
                name: name.clone(),
                metadata: file.metadata()?,
                file,
            });
        }
        let snapshot_names = snapshots.names_bounded_migration(MAX_SNAPSHOT_ENTRIES)?;
        for name in &snapshot_names {
            snapshot_identity(name)?;
            let file = snapshots.open(name, false, false)?;
            files.push(LocalFilePin {
                snapshot: true,
                name: name.clone(),
                metadata: file.metadata()?,
                file,
            });
        }
        let find = |name: &str| {
            files
                .iter()
                .find(|pin| !pin.snapshot && pin.name == name)
                .ok_or(StoreError::RecoveryRequired)
        };
        let state: ConversationStateV2 = serde_json::from_slice(&read_pin(find("state.json")?)?)?;
        state.validate()?;
        if state.session_id != session {
            return Err(StoreError::RecoveryRequired);
        }
        let raw = read_pin(find("raw.jsonl")?)?;
        if state.raw_commit_offset != raw.len() as u64 {
            return Err(StoreError::RecoveryRequired);
        }
        let current = conversation::decode_raw_snapshot(state, raw)?;
        classify(&current)?;
        for pin in files.iter().filter(|pin| pin.snapshot) {
            let (generation, hash) = snapshot_identity(&pin.name)?;
            let snapshot = conversation::decode_saved_snapshot(
                &read_pin(pin)?,
                &current.state.project_id,
                session,
                generation,
                hash,
            )?;
            conversation::validate_migration_snapshot(&snapshot)?;
            classify(&snapshot)?;
        }
        for pin in files.iter().filter(|pin| !pin.snapshot) {
            match pin.name.as_str() {
                "rewind-branch-v1.json" => {
                    let metadata: conversation::RewindBranchMetadataV1 =
                        serde_json::from_slice(&read_pin(pin)?)?;
                    conversation::validate_rewind_branch_metadata(&metadata)?;
                }
                "workspace-head-binding-v1.json" => {
                    let binding: conversation::WorkspaceHeadBindingV1 =
                        serde_json::from_slice(&read_pin(pin)?)?;
                    conversation::validate_workspace_head_binding(&binding)?;
                    if binding.project_id != current.state.project_id
                        || binding.session_id != session
                    {
                        return Err(StoreError::RecoveryRequired);
                    }
                }
                "rewind-boundaries-v1.json" => {
                    for boundary in conversation::decode_rewind_boundaries(&read_pin(pin)?)? {
                        if boundary.project_id != current.state.project_id
                            || boundary.session_id != session
                        {
                            return Err(StoreError::RecoveryRequired);
                        }
                    }
                }
                _ => (),
            }
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_LOCAL_DECODE.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let view = Self {
            parent,
            container,
            container_metadata,
            session: session.into(),
            directory,
            snapshots,
            directories: vec![parent_metadata, directory_metadata, snapshots_metadata],
            names,
            snapshot_names,
            mount,
            files,
            current,
        };
        view.verify_current()?;
        Ok(view)
    }
    pub(in crate::desktop_store) fn verified_snapshot(&self) -> StoreResult<&ConversationSnapshot> {
        self.verify_current()?;
        Ok(&self.current)
    }
    pub(in crate::desktop_store) fn verify_current(&self) -> StoreResult<()> {
        self.verify_directories()?;
        for pin in &self.files {
            let directory = if pin.snapshot {
                &self.snapshots
            } else {
                &self.directory
            };
            let named = directory.open(&pin.name, false, false)?;
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
        self.parent.verify()?;
        verify_parent_identity(
            &self.parent.retained_file_m2a()?,
            polaris_tools::entry_kernel::EntryAnchorV2::of(&self.directories[0]),
            &self.mount,
        )?;
        self.verify_container()?;
        #[cfg(test)]
        AFTER_CONTAINER_CHECK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        for (directory, metadata) in [&self.directory, &self.snapshots]
            .into_iter()
            .zip(&self.directories[1..])
        {
            directory.verify()?;
            let file = directory.retained_file_m2a()?;
            if native_mount(&file)? != *self.mount
                || file_stamp(&file.metadata()?) != file_stamp(metadata)
            {
                return Err(StoreError::RecoveryRequired);
            }
        }
        if self.directory.names_bounded_migration(7)? != self.names
            || self
                .snapshots
                .names_bounded_migration(self.snapshot_names.len())?
                != self.snapshot_names
        {
            return Err(StoreError::RecoveryRequired);
        }
        for (directory, metadata) in [&self.directory, &self.snapshots]
            .into_iter()
            .zip(&self.directories[1..])
        {
            directory.verify()?;
            if file_stamp(&directory.retained_file_m2a()?.metadata()?) != file_stamp(metadata) {
                return Err(StoreError::RecoveryRequired);
            }
        }
        self.verify_container()?;
        self.parent.verify()?;
        verify_parent_identity(
            &self.parent.retained_file_m2a()?,
            polaris_tools::entry_kernel::EntryAnchorV2::of(&self.directories[0]),
            &self.mount,
        )
    }
    fn verify_container(&self) -> StoreResult<()> {
        let (container, directory) = self
            .parent
            .existing_local_session_migration(&self.session)?;
        disk::same(
            &directory.retained_file_m2a()?,
            &self.directory.retained_file_m2a()?,
        )?;
        for file in [&self.container, &container] {
            if native_mount(file)? != *self.mount
                || file_stamp(&file.metadata()?) != file_stamp(&self.container_metadata)
            {
                return Err(StoreError::RecoveryRequired);
            }
        }
        Ok(())
    }
}
fn snapshot_identity(name: &str) -> StoreResult<(u64, &str)> {
    let (generation, hash) = name
        .strip_suffix(".json")
        .and_then(|name| name.split_once('-'))
        .ok_or(StoreError::RecoveryRequired)?;
    let generation = generation
        .parse::<u64>()
        .map_err(|_| StoreError::RecoveryRequired)?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || name != format!("{generation}-{hash}.json")
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok((generation, hash))
}
fn read_pin(pin: &LocalFilePin) -> StoreResult<Vec<u8>> {
    if pin.metadata.len() > MAX_FILE_BYTES {
        return Err(StoreError::Capacity("local migration file bytes"));
    }
    let length = usize::try_from(pin.metadata.len()).map_err(|_| StoreError::RecoveryRequired)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| StoreError::Capacity("local migration bytes"))?;
    let mut file = pin.file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    Read::by_ref(&mut file)
        .take(
            pin.metadata
                .len()
                .checked_add(1)
                .ok_or(StoreError::Overflow)?,
        )
        .read_to_end(&mut bytes)?;
    if bytes.len() != length {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(bytes)
}
fn classify(snapshot: &ConversationSnapshot) -> StoreResult<()> {
    let mut first = 0;
    while first < snapshot.events.len() {
        let key = (snapshot.events[first].epoch, snapshot.events[first].turn_id);
        let end = first
            + snapshot.events[first..]
                .iter()
                .take_while(|event| (event.epoch, event.turn_id) == key)
                .count();
        validate_local_turn(
            &snapshot.events[first..end],
            &snapshot.state.project_id,
            &snapshot.state.session_id,
        )?;
        first = end;
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    static AFTER_LOCAL_CONTAINER_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
    static AFTER_LOCAL_CONTAINER_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
    static AFTER_LOCAL_CONTAINER_FINAL_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
    static AFTER_CONTAINER_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
    pub(in crate::desktop_store) static AFTER_LOCAL_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_state::ConversationStore;
    use polaris_provider::{Message, Role, ToolCall};
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    const ID: &str = "00000000-0000-4000-8000-000000000001";
    fn setup() -> (
        tempfile::TempDir,
        ConversationStore,
        disk::OwnerControlledContainer,
    ) {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = ConversationStore::create(
            temp.path(),
            ConversationStateV2::new("project".into(), ID.into()).unwrap(),
        )
        .unwrap();
        let parent = disk::OwnerControlledContainer::open_migration(temp.path()).unwrap();
        (temp, store, parent)
    }
    fn snapshot(
        path: &std::path::Path,
    ) -> Vec<(String, Vec<u8>, (u64, u64, u32, u64, i64, i64, i64, i64))> {
        fn visit(
            root: &std::path::Path,
            path: &std::path::Path,
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
        visit(path, path, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
    fn events() -> Vec<RawEventV2> {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![ToolCall {
            id: "call".into(),
            name: "ordinary".into(),
            arguments: serde_json::json!({}),
        }];
        vec![
            RawEventV2 {
                schema_version: 2,
                sequence: 1,
                epoch: 0,
                turn_id: 1,
                starts_turn: true,
                message: Message::user("question"),
            },
            RawEventV2 {
                schema_version: 2,
                sequence: 2,
                epoch: 0,
                turn_id: 1,
                starts_turn: false,
                message: assistant,
            },
        ]
    }
    fn write_raw(store: &ConversationStore, events: &[RawEventV2], epoch: u64) {
        let mut raw = Vec::new();
        for event in events {
            serde_json::to_writer(&mut raw, event).unwrap();
            raw.push(b'\n');
        }
        let mut state = ConversationStateV2::new("project".into(), ID.into()).unwrap();
        state.epoch = epoch;
        state.raw_commit_offset = raw.len() as u64;
        state.raw_commit_hash = content_hash(&raw);
        fs::write(store.directory().join("raw.jsonl"), raw).unwrap();
        fs::write(
            store.directory().join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn migration_local_fd_read_only_normal_container_and_existing_lock() {
        for (root_mode, mode) in [(0o700, 0o700), (0o755, 0o755), (0o750, 0o750)] {
            let (_temp, store, parent) = setup();
            fs::set_permissions(parent.path(), fs::Permissions::from_mode(root_mode)).unwrap();
            fs::set_permissions(
                parent.path().join("sessions-v2"),
                fs::Permissions::from_mode(mode),
            )
            .unwrap();
            let mut lock = store.lock().unwrap();
            let data = lock.append(Message::user("question"), true).unwrap();
            lock.save_snapshot(&data).unwrap();
            lock.publish(&data, data.state.clone()).unwrap();
            drop(lock);
            let published = { store.lock().unwrap().snapshot().unwrap() };
            store
                .bind_workspace_head(&published, &"a".repeat(32))
                .unwrap();
            let before = snapshot(parent.path());
            let view = LocalSessionTransportInspectionV2::inspect(&parent, ID).unwrap();
            assert_eq!(view.verified_snapshot().unwrap().events.len(), 1);
            assert_eq!(snapshot(parent.path()), before);
            assert!(store.lock().is_err());
            drop(view);
            store.lock().unwrap();
        }
    }
    #[test]
    fn migration_local_fd_rejects_active_writer_without_recovery() {
        let (_temp, store, parent) = setup();
        let _lock = store.lock().unwrap();
        let before = snapshot(parent.path());
        assert!(matches!(
            LocalSessionTransportInspectionV2::inspect(&parent, ID),
            Err(StoreError::Busy)
        ));
        assert_eq!(snapshot(parent.path()), before);
    }
    #[test]
    fn migration_local_fd_classifies_expired_turns_without_repair() {
        let (_temp, store, parent) = setup();
        let mut data = events();
        data.push(RawEventV2 {
            schema_version: 2,
            sequence: 3,
            epoch: 1,
            turn_id: 2,
            starts_turn: true,
            message: Message::user("current"),
        });
        write_raw(&store, &data, 1);
        let before = snapshot(parent.path());
        assert!(matches!(
            LocalSessionTransportInspectionV2::inspect(&parent, ID),
            Err(StoreError::RecoveryRequired)
        ));
        assert_eq!(snapshot(parent.path()), before);
    }
    #[test]
    fn migration_local_fd_classifies_saved_snapshot_independently() {
        let (_temp, store, parent) = setup();
        let pending = events();
        let mut complete = pending.clone();
        complete.push(RawEventV2 {
            schema_version: 2,
            sequence: 3,
            epoch: 0,
            turn_id: 1,
            starts_turn: false,
            message: Message {
                role: Role::Tool,
                content: "done".into(),
                tool_call_id: Some("call".into()),
                tool_calls: vec![],
                ..Message::user("")
            },
        });
        write_raw(&store, &complete, 0);
        LocalSessionTransportInspectionV2::inspect(&parent, ID).unwrap();
        let mut raw = Vec::new();
        for event in &pending {
            serde_json::to_writer(&mut raw, event).unwrap();
            raw.push(b'\n');
        }
        let saved = conversation::decode_raw_snapshot(
            ConversationStateV2::new("project".into(), ID.into()).unwrap(),
            raw,
        )
        .unwrap();
        let path = store
            .directory()
            .join("snapshots")
            .join(format!("0-{}.json", saved.raw_hash));
        fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let before = snapshot(parent.path());
        assert!(matches!(
            LocalSessionTransportInspectionV2::inspect(&parent, ID),
            Err(StoreError::RecoveryRequired)
        ));
        assert_eq!(snapshot(parent.path()), before);
    }
    #[test]
    fn migration_local_fd_rejects_tails_unknown_files_and_foreign_state() {
        for mode in 0..4 {
            let (_temp, store, parent) = setup();
            match mode {
                0 => {
                    fs::OpenOptions::new()
                        .append(true)
                        .open(store.directory().join("raw.jsonl"))
                        .unwrap()
                        .write_all(b"unpublished\n")
                        .unwrap();
                }
                1 => fs::write(store.directory().join("state.next"), b"unfinished").unwrap(),
                2 => {
                    fs::write(store.directory().join("snapshots/invalid.json"), b"unknown").unwrap()
                }
                _ => {
                    let state = ConversationStateV2::new(
                        "project".into(),
                        "00000000-0000-4000-8000-000000000002".into(),
                    )
                    .unwrap();
                    fs::write(
                        store.directory().join("state.json"),
                        serde_json::to_vec(&state).unwrap(),
                    )
                    .unwrap();
                }
            }
            let before = snapshot(parent.path());
            assert!(LocalSessionTransportInspectionV2::inspect(&parent, ID).is_err());
            assert_eq!(snapshot(parent.path()), before);
        }
    }
    #[test]
    fn migration_local_fd_rejects_file_container_and_snapshot_drift() {
        for mode in 0..4 {
            let (_temp, store, parent) = setup();
            let view = LocalSessionTransportInspectionV2::inspect(&parent, ID).unwrap();
            match mode {
                0 => {
                    fs::OpenOptions::new()
                        .append(true)
                        .open(store.directory().join("raw.jsonl"))
                        .unwrap()
                        .write_all(b"changed")
                        .unwrap();
                }
                1 => fs::set_permissions(
                    parent.path().join("sessions-v2"),
                    fs::Permissions::from_mode(0o775),
                )
                .unwrap(),
                2 => fs::write(store.directory().join("snapshots/added"), b"changed").unwrap(),
                _ => {
                    let path = store.directory().join("replacement");
                    fs::write(
                        &path,
                        fs::read(store.directory().join("state.json")).unwrap(),
                    )
                    .unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                    fs::rename(path, store.directory().join("state.json")).unwrap();
                }
            }
            assert!(view.verified_snapshot().is_err());
        }
    }
    #[test]
    fn migration_local_fd_closes_changes_after_decode() {
        let (_temp, store, parent) = setup();
        let path = store.directory().join("raw.jsonl");
        AFTER_LOCAL_DECODE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(b"changed")
                    .unwrap();
            }))
        });
        assert!(LocalSessionTransportInspectionV2::inspect(&parent, ID).is_err());
        AFTER_LOCAL_DECODE.with(|slot| assert!(slot.borrow().is_none()));
    }
    #[test]
    fn migration_local_fd_rejects_oversized_file_before_read_and_writable_roots() {
        let (_temp, store, parent) = setup();
        let raw = fs::OpenOptions::new()
            .write(true)
            .open(store.directory().join("raw.jsonl"))
            .unwrap();
        raw.set_len(MAX_FILE_BYTES + 1).unwrap();
        let metadata = raw.metadata().unwrap();
        let pin = LocalFilePin {
            snapshot: false,
            name: "raw.jsonl".into(),
            file: raw,
            metadata,
        };
        assert!(matches!(read_pin(&pin), Err(StoreError::Capacity(_))));
        assert!(matches!(
            LocalSessionTransportInspectionV2::inspect(&parent, ID),
            Err(StoreError::Capacity(_))
        ));
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(disk::OwnerControlledContainer::open_migration(parent.path()).is_err());
        assert_eq!(
            fs::metadata(store.directory().join("raw.jsonl"))
                .unwrap()
                .len(),
            MAX_FILE_BYTES + 1
        );
    }
    #[test]
    fn migration_local_fd_closes_sibling_change_after_container_check() {
        let (_temp, _store, parent) = setup();
        let sibling = parent.path().join("sessions-v2/added");
        AFTER_CONTAINER_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::create_dir(sibling).unwrap();
            }))
        });
        assert!(matches!(
            LocalSessionTransportInspectionV2::inspect(&parent, ID),
            Err(StoreError::RecoveryRequired)
        ));
        AFTER_CONTAINER_CHECK.with(|slot| assert!(slot.borrow().is_none()));
    }
    #[test]
    fn migration_local_fd_rejects_saved_commit_inside_json_line() {
        let (_temp, store, parent) = setup();
        let mut lock = store.lock().unwrap();
        let mut saved = lock.append(Message::user("question"), true).unwrap();
        lock.publish(&saved, saved.state.clone()).unwrap();
        drop(lock);
        let raw = fs::read(store.directory().join("raw.jsonl")).unwrap();
        saved.state.raw_commit_offset = 1;
        saved.state.raw_commit_hash = content_hash(&raw[..1]);
        let path = store.directory().join("snapshots").join(format!(
            "{}-{}.json",
            saved.state.generation, saved.raw_hash
        ));
        fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        store
            .load_snapshot(saved.state.generation, &saved.raw_hash)
            .unwrap();
        let before = snapshot(parent.path());
        assert!(LocalSessionTransportInspectionV2::inspect(&parent, ID).is_err());
        assert_eq!(snapshot(parent.path()), before);
    }
    const OTHER_ID: &str = "00000000-0000-4000-8000-000000000002";
    #[test]
    fn migration_local_container_absent_and_empty_are_read_only() {
        for exists in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o750)).unwrap();
            if exists {
                fs::create_dir(temp.path().join("sessions-v2")).unwrap();
                fs::set_permissions(
                    temp.path().join("sessions-v2"),
                    fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            fs::write(temp.path().join("unrelated-settings"), b"unchanged").unwrap();
            let parent = disk::OwnerControlledContainer::open_migration(temp.path()).unwrap();
            let before = snapshot(temp.path());
            let root_before = file_stamp(&fs::metadata(temp.path()).unwrap());
            let view = LocalSessionsTransportInspectionV2::inspect(&parent).unwrap();
            assert!(view.verified_sessions().unwrap().is_empty());
            assert_eq!(view.container.is_some(), exists);
            assert_eq!(snapshot(temp.path()), before);
            assert_eq!(file_stamp(&fs::metadata(temp.path()).unwrap()), root_before);
            if !exists {
                assert!(!temp.path().join("sessions-v2").exists());
            }
        }
    }
    #[test]
    fn migration_local_container_holds_all_sessions_across_projects() {
        let (_temp, store, parent) = setup();
        let other = ConversationStore::create(
            parent.path(),
            ConversationStateV2::new("other-project".into(), OTHER_ID.into()).unwrap(),
        )
        .unwrap();
        let before = snapshot(parent.path());
        let view = LocalSessionsTransportInspectionV2::inspect(&parent).unwrap();
        assert_eq!(
            view.verified_sessions()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [ID, OTHER_ID]
        );
        assert_eq!(
            view.verified_sessions().unwrap()[OTHER_ID]
                .verified_snapshot()
                .unwrap()
                .state
                .project_id,
            "other-project"
        );
        assert!(store.lock().is_err());
        assert!(other.lock().is_err());
        assert_eq!(snapshot(parent.path()), before);
        drop(view);
        store.lock().unwrap();
        other.lock().unwrap();
    }
    #[test]
    fn migration_local_container_later_busy_releases_prior_owner() {
        let (_temp, store, parent) = setup();
        let other = ConversationStore::create(
            parent.path(),
            ConversationStateV2::new("project".into(), OTHER_ID.into()).unwrap(),
        )
        .unwrap();
        let _lock = other.lock().unwrap();
        let before = snapshot(parent.path());
        assert!(matches!(
            LocalSessionsTransportInspectionV2::inspect(&parent),
            Err(StoreError::Busy)
        ));
        store.lock().unwrap();
        assert_eq!(snapshot(parent.path()), before);
    }
    #[test]
    fn migration_local_container_later_pending_is_not_skipped_or_repaired() {
        let (_temp, store, parent) = setup();
        let other = ConversationStore::create(
            parent.path(),
            ConversationStateV2::new("project".into(), OTHER_ID.into()).unwrap(),
        )
        .unwrap();
        let mut lock = other.lock().unwrap();
        let _user = lock.append(Message::user("question"), true).unwrap();
        let mut assistant = Message::assistant("");
        assistant.tool_calls = events()[1].message.tool_calls.clone();
        let data = lock.append(assistant, false).unwrap();
        lock.publish(&data, data.state.clone()).unwrap();
        drop(lock);
        let before = snapshot(parent.path());
        assert!(matches!(
            LocalSessionsTransportInspectionV2::inspect(&parent),
            Err(StoreError::RecoveryRequired)
        ));
        assert_eq!(snapshot(parent.path()), before);
        store.lock().unwrap();
        other.lock().unwrap();
    }
    #[test]
    fn migration_local_container_rejects_unknown_file_symlink_and_missing_state() {
        for kind in 0..4 {
            let (_temp, _store, parent) = setup();
            let entry = parent.path().join("sessions-v2").join(if kind == 0 {
                "unknown"
            } else {
                OTHER_ID
            });
            match kind {
                0 | 3 => {
                    fs::create_dir(&entry).unwrap();
                    fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();
                }
                1 => fs::write(&entry, b"not a session").unwrap(),
                _ => std::os::unix::fs::symlink(parent.path().join("sessions-v2").join(ID), &entry)
                    .unwrap(),
            }
            // The symlink's contents are not traversed by this test snapshot.
            let before = file_stamp(&fs::symlink_metadata(&entry).unwrap());
            assert!(LocalSessionsTransportInspectionV2::inspect(&parent).is_err());
            assert_eq!(file_stamp(&fs::symlink_metadata(&entry).unwrap()), before);
            assert!(entry.symlink_metadata().is_ok());
        }
    }
    #[test]
    fn migration_local_container_rejects_namespace_inode_and_mode_drift() {
        for kind in 0..4 {
            let (_temp, _store, parent) = setup();
            let view = LocalSessionsTransportInspectionV2::inspect(&parent).unwrap();
            let container = parent.path().join("sessions-v2");
            match kind {
                0 => fs::create_dir(container.join(OTHER_ID)).unwrap(),
                1 => {
                    fs::rename(&container, parent.path().join("old-sessions-v2")).unwrap();
                    fs::create_dir(&container).unwrap();
                    fs::set_permissions(&container, fs::Permissions::from_mode(0o700)).unwrap();
                }
                2 => fs::set_permissions(&container, fs::Permissions::from_mode(0o775)).unwrap(),
                _ => fs::rename(container.join(ID), container.join(OTHER_ID)).unwrap(),
            }
            assert!(view.verified_sessions().is_err());
        }
    }
    #[test]
    fn migration_local_container_parent_siblings_do_not_change_local_inventory() {
        for populated in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let store = if populated {
                Some(
                    ConversationStore::create(
                        temp.path(),
                        ConversationStateV2::new("project".into(), ID.into()).unwrap(),
                    )
                    .unwrap(),
                )
            } else {
                None
            };
            let parent = disk::OwnerControlledContainer::open_migration(temp.path()).unwrap();
            let view = LocalSessionsTransportInspectionV2::inspect(&parent).unwrap();
            fs::write(temp.path().join("fixture-intent"), b"unrelated sibling").unwrap();
            fs::create_dir(temp.path().join("fixture-staging")).unwrap();
            view.verify_current().unwrap();
            if let Some(store) = &store {
                assert!(store.lock().is_err());
            }
            if !populated {
                fs::create_dir(temp.path().join("sessions-v2")).unwrap();
            } else {
                fs::create_dir(temp.path().join("sessions-v2").join(OTHER_ID)).unwrap();
            }
            assert!(view.verify_current().is_err());
        }
    }
    #[test]
    fn migration_local_container_retains_parent_mode_and_named_identity() {
        for replacement in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            fs::create_dir(&data).unwrap();
            fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
            let _store = ConversationStore::create(
                &data,
                ConversationStateV2::new("project".into(), ID.into()).unwrap(),
            )
            .unwrap();
            let parent = disk::OwnerControlledContainer::open_migration(&data).unwrap();
            let view = LocalSessionsTransportInspectionV2::inspect(&parent).unwrap();
            if replacement {
                // Replace the data-root name while keeping the old FD alive.
                fs::rename(&data, temp.path().join("retained")).unwrap();
                fs::create_dir(&data).unwrap();
                fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
                assert!(view.verify_current().is_err());
            } else {
                fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o750)).unwrap();
                assert!(view.verify_current().is_err());
            }
        }
    }
    #[test]
    fn migration_local_container_closes_constructor_and_final_enumeration_changes() {
        for kind in 0..3 {
            let (_temp, _store, parent) = setup();
            let path = parent.path().join("sessions-v2").join(OTHER_ID);
            let hook: Box<dyn FnOnce()> = Box::new(move || fs::create_dir(path).unwrap());
            match kind {
                0 => AFTER_LOCAL_CONTAINER_NAMES.with(|slot| *slot.borrow_mut() = Some(hook)),
                1 => AFTER_LOCAL_CONTAINER_DECODE.with(|slot| *slot.borrow_mut() = Some(hook)),
                _ => AFTER_LOCAL_CONTAINER_FINAL_NAMES.with(|slot| *slot.borrow_mut() = Some(hook)),
            }
            assert!(LocalSessionsTransportInspectionV2::inspect(&parent).is_err());
            AFTER_LOCAL_CONTAINER_NAMES.with(|slot| assert!(slot.borrow().is_none()));
            AFTER_LOCAL_CONTAINER_DECODE.with(|slot| assert!(slot.borrow().is_none()));
            AFTER_LOCAL_CONTAINER_FINAL_NAMES.with(|slot| assert!(slot.borrow().is_none()));
        }
    }
    #[test]
    fn migration_local_container_mount_and_complete_inventory_limits_fail_closed() {
        let (_temp, _store, parent) = setup();
        let container = parent.local_container_migration().unwrap().unwrap();
        assert!(matches!(
            container.names_bounded_migration(0),
            Err(StoreError::Capacity(_))
        ));
        let file = container.retained_file_m2a().unwrap();
        let metadata = file.metadata().unwrap();
        REJECT_MOUNT_FILE.with(|slot| slot.set(Some((metadata.dev(), metadata.ino()))));
        let result = LocalSessionsTransportInspectionV2::inspect(&parent);
        REJECT_MOUNT_FILE.with(|slot| slot.set(None));
        assert!(matches!(result, Err(StoreError::RecoveryRequired)));
        LocalSessionsTransportInspectionV2::inspect(&parent).unwrap();
    }
}
