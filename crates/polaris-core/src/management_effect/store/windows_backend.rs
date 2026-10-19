//! Qualified Windows I/O for the existing management journal algorithms.

use super::*;
use polaris_auth::{
    AuthError,
    protection::{
        WindowsFamilyDirectory, WindowsFamilyFile, WindowsFileIdentity, WindowsPinnedDirectory,
    },
};
use std::{
    ffi::OsStr,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

type Result<T> = std::result::Result<T, ManagementEffectStoreError>;
type Version = (WindowsFileIdentity, u64, i64, i64);

fn error(error: AuthError) -> ManagementEffectStoreError {
    match error {
        AuthError::Io(error) => error.into(),
        AuthError::Busy => std::io::Error::from(std::io::ErrorKind::WouldBlock).into(),
        _ => ManagementEffectStoreError::InvalidStore,
    }
}
fn missing(error: &ManagementEffectStoreError) -> bool {
    matches!(error, ManagementEffectStoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
}
pub(super) fn require_inventory(request: &PrepareManagementEffectV1) -> Result<()> {
    if matches!(
        request.inventory.encoded().as_str(),
        "C08" | "C10" | "C12" | "D27"
    ) {
        return Err(unsupported());
    }
    Ok(())
}

pub(super) struct WindowsStoreV1 {
    _project: Option<WindowsPinnedDirectory>,
    state: Arc<WindowsFamilyDirectory>,
    root: Arc<WindowsFamilyDirectory>,
    path: PathBuf,
}
impl std::fmt::Debug for WindowsStoreV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsStoreV1").finish_non_exhaustive()
    }
}
impl WindowsStoreV1 {
    fn directory(&self, path: &Path) -> Result<Arc<WindowsFamilyDirectory>> {
        if let Some(project) = &self._project {
            project.revalidate().map_err(error)?;
        }
        self.state.revalidate().map_err(error)?;
        self.root.revalidate().map_err(error)?;
        let relative = path
            .strip_prefix(&self.path)
            .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
        let mut directory = self.root.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(ManagementEffectStoreError::InvalidStore);
            };
            directory = Arc::new(directory.child(name, false).map_err(error)?);
        }
        Ok(directory)
    }
    fn parent<'a>(&self, path: &'a Path) -> Result<(Arc<WindowsFamilyDirectory>, &'a OsStr)> {
        let parent = path
            .parent()
            .ok_or(ManagementEffectStoreError::InvalidStore)?;
        Ok((
            self.directory(parent)?,
            path.file_name()
                .ok_or(ManagementEffectStoreError::InvalidStore)?,
        ))
    }
    pub(super) fn open(&self, path: &Path, write: bool) -> Result<WindowsFamilyFile> {
        let (parent, name) = self.parent(path)?;
        if !write && name == OsStr::new("CURRENT") {
            parent.open_mutable_file(name).map_err(error)
        } else {
            parent.open_file(name, write, false).map_err(error)
        }
    }
    pub(super) fn read(&self, path: &Path, maximum: u64) -> Result<Vec<u8>> {
        bytes(&self.open(path, false)?, maximum)
    }
    pub(super) fn ensure_directory(&self, path: &Path) -> Result<()> {
        match self.directory(path) {
            Ok(_) => Ok(()),
            Err(value) if missing(&value) => {
                let (parent, name) = self.parent(path)?;
                parent.child(name, true).map_err(error)?;
                parent.sync_all().map_err(error)
            }
            Err(value) => Err(value),
        }
    }
    pub(super) fn write_new(&self, path: &Path, body: &[u8]) -> Result<()> {
        let (parent, name) = self.parent(path)?;
        let mut file = parent.open_file(name, true, true).map_err(error)?;
        file.write_all(body)?;
        file.sync_all()?;
        Ok(())
    }
    pub(super) fn sync(&self, path: &Path) -> Result<()> {
        self.directory(path)?.sync_all().map_err(error)
    }
    pub(super) fn directory_present(&self, path: &Path) -> Result<bool> {
        match self.directory(path) {
            Ok(_) => Ok(true),
            Err(value) if missing(&value) => Ok(false),
            Err(value) => Err(value),
        }
    }
    pub(super) fn rename_directory(&self, source: &Path, target: &Path) -> Result<()> {
        let (parent, source_name) = self.parent(source)?;
        let (other, target_name) = self.parent(target)?;
        if parent.identity() != other.identity() {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        parent
            .rename_child_no_clobber(source_name, target_name)
            .map_err(error)
    }
    pub(super) fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        let (parent, source_name) = self.parent(source)?;
        let (other, target_name) = self.parent(target)?;
        if parent.identity() != other.identity() {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        parent.replace(source_name, target_name).map_err(error)
    }
}

#[derive(Debug)]
pub(super) struct PinnedManagementFileV1 {
    parent: Arc<WindowsFamilyDirectory>,
    name: String,
    pub(super) file: WindowsFamilyFile,
    pub(super) evidence: ManagementEffectInspectedFileV1,
    pub(super) bytes: Vec<u8>,
    version: Version,
}
fn bytes(file: &WindowsFamilyFile, maximum: u64) -> Result<Vec<u8>> {
    let before = file.version()?;
    if before.1 > maximum {
        return Err(ManagementEffectStoreError::StoreFull);
    }
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut body = Vec::new();
    reader
        .take(maximum.saturating_add(1))
        .read_to_end(&mut body)?;
    if body.len() as u64 != before.1 || file.version()? != before {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(body)
}
fn pin(
    parent: Arc<WindowsFamilyDirectory>,
    name: &str,
    relative: &str,
    maximum: u64,
) -> Result<PinnedManagementFileV1> {
    let file = if name == "CURRENT" {
        parent.open_mutable_file(OsStr::new(name))
    } else {
        parent.open_file(OsStr::new(name), false, false)
    }
    .map_err(error)?;
    let version = file.version()?;
    let body = bytes(&file, maximum)?;
    let (device, inode) = file.legacy_identity()?;
    let value = PinnedManagementFileV1 {
        parent,
        name: name.into(),
        file,
        version,
        evidence: ManagementEffectInspectedFileV1 {
            relative_name: relative.into(),
            length: body.len() as u64,
            hash: ManagementEffectHashV1::digest(&body),
            device,
            inode,
        },
        bytes: body,
    };
    verify_file(&value)?;
    Ok(value)
}
fn verify_file(pin: &PinnedManagementFileV1) -> Result<()> {
    let named = if pin.name == "CURRENT" {
        pin.parent.open_mutable_file(OsStr::new(&pin.name))
    } else {
        pin.parent.open_file(OsStr::new(&pin.name), false, false)
    }
    .map_err(error)?;
    if named.version()? != pin.version
        || pin.file.version()? != pin.version
        || bytes(&pin.file, pin.evidence.length)? != pin.bytes
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}
fn inventory(
    root: Arc<WindowsFamilyDirectory>,
    limits: ManagementEffectLimitsV1,
) -> Result<ManagementInventoryV1> {
    let mut value = ManagementInventoryV1 {
        files: BTreeMap::new(),
        directories: BTreeMap::new(),
        migration_directories: BTreeMap::new(),
        windows_directories: BTreeMap::new(),
    };
    for entry in root.entries(65_536).map_err(error)? {
        let name = entry
            .name
            .into_string()
            .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
        if name == "generations" {
            let generations = Arc::new(
                entry
                    .child
                    .ok_or(ManagementEffectStoreError::InvalidStore)?,
            );
            value
                .directories
                .insert(name.clone(), generations.legacy_identity().map_err(error)?);
            value
                .windows_directories
                .insert(name.clone(), generations.clone());
            for child in generations.entries(65_536).map_err(error)? {
                let number = child
                    .name
                    .into_string()
                    .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
                let decimal = number
                    .strip_prefix(".seal-")
                    .and_then(|n| n.strip_suffix(".v1.tmp"))
                    .or_else(|| {
                        number
                            .strip_prefix(".orphan-")
                            .and_then(|n| n.split_once('-'))
                            .map(|(n, _)| n)
                    })
                    .unwrap_or(&number);
                if decimal
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|n| n == 0 || n.to_string() != decimal)
                {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
                let directory = Arc::new(
                    child
                        .child
                        .ok_or(ManagementEffectStoreError::InvalidStore)?,
                );
                let relative = format!("generations/{number}");
                value.directories.insert(
                    relative.clone(),
                    directory.legacy_identity().map_err(error)?,
                );
                value
                    .windows_directories
                    .insert(relative.clone(), directory.clone());
                for file in directory.entries(4).map_err(error)? {
                    if file.child.is_some() {
                        return Err(ManagementEffectStoreError::InvalidStore);
                    }
                    let name = file
                        .name
                        .into_string()
                        .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
                    let maximum = match name.as_str() {
                        "archive.v1.json" => 64 * 1024 * 1024,
                        "checkpoint.v1.json" => 64 * 1024,
                        "manifest.v1.json" => 4096,
                        JOURNAL_NAME => limits.max_journal_bytes,
                        _ => return Err(ManagementEffectStoreError::CorruptJournal),
                    };
                    let relative = format!("{relative}/{name}");
                    value.files.insert(
                        relative.clone(),
                        pin(directory.clone(), &name, &relative, maximum)?,
                    );
                }
            }
        } else {
            if entry.child.is_some() {
                return Err(ManagementEffectStoreError::InvalidStore);
            }
            let maximum = match name.as_str() {
                OWNER_LOCK_NAME => 4096,
                JOURNAL_NAME => limits.max_journal_bytes,
                "CURRENT" => 4096,
                other if other.starts_with(".CURRENT-") && other.len() < 128 => 4096,
                _ => return Err(ManagementEffectStoreError::CorruptJournal),
            };
            value
                .files
                .insert(name.clone(), pin(root.clone(), &name, &name, maximum)?);
        }
    }
    for file in value.files.values() {
        verify_file(file)?;
    }
    root.revalidate().map_err(error)?;
    Ok(value)
}

impl ManagementEffectStoreV1 {
    fn windows(&self) -> Result<&WindowsStoreV1> {
        self.windows.as_deref().ok_or_else(unsupported)
    }
    pub fn open(project: &Path, lease: &ProjectMutationLeaseV1) -> Result<Self> {
        let canonical = fs::canonicalize(project)?;
        let state = crate::project::storage_root_from_env()?
            .join("state")
            .join(crate::project::project_id(&canonical));
        Self::open_at_state_dir(project, &state, lease)
    }
    pub fn open_at_state_dir(
        project: &Path,
        state: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<Self> {
        let project_pin = WindowsPinnedDirectory::open_existing(project).map_err(error)?;
        let canonical = fs::canonicalize(project)?;
        let named = WindowsPinnedDirectory::open_existing(&canonical).map_err(error)?;
        if named.identity() != project_pin.identity() {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let scope = ManagementEffectHashV1::digest(canonical.as_os_str().as_encoded_bytes());
        let state = state.to_owned();
        Self::windows_open(
            &state,
            scope,
            lease,
            ManagementEffectLimitsV1::default(),
            true,
            Some(project_pin),
        )?
        .ok_or(ManagementEffectStoreError::InvalidStore)
    }
    pub(super) fn open_at(
        state: &Path,
        expected: PathBuf,
        scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<Self> {
        if lease.store_root() != expected {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        Self::windows_open(state, scope, lease, limits, true, None)?
            .ok_or(ManagementEffectStoreError::InvalidStore)
    }
    fn windows_open(
        state: &Path,
        scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
        create: bool,
        project: Option<WindowsPinnedDirectory>,
    ) -> Result<Option<Self>> {
        validate_limits(limits)?;
        crate::project_mutation::ProjectReadLeaseV1::Mutation(lease)
            .verify(state)
            .map_err(|_| ManagementEffectStoreError::WrongProjectLease)?;
        let state_pin = Arc::new(WindowsFamilyDirectory::open_existing(state).map_err(error)?);
        let (root, fresh) = match state_pin
            .child(OsStr::new(STORE_NAME), false)
            .map_err(error)
        {
            Ok(root) => (root, false),
            Err(value) if missing(&value) && !create => return Ok(None),
            Err(value) if missing(&value) => (
                state_pin
                    .child(OsStr::new(STORE_NAME), true)
                    .map_err(error)?,
                true,
            ),
            Err(value) => return Err(value),
        };
        if fresh {
            for name in [JOURNAL_NAME, OWNER_LOCK_NAME] {
                root.open_file(OsStr::new(name), true, true)
                    .map_err(error)?
                    .sync_all()?;
            }
            root.sync_all().map_err(error)?;
            state_pin.sync_all().map_err(error)?;
        }
        let root = Arc::new(root);
        let root_path = state.join(STORE_NAME);
        let store = Self {
            state_dir: state.into(),
            root: root_path.clone(),
            journal_path: root_path.join(JOURNAL_NAME),
            expected_lease_root: state.join("rewind-workspace"),
            project_scope: scope,
            limits,
            windows: Some(Arc::new(WindowsStoreV1 {
                _project: project,
                state: state_pin,
                root,
                path: root_path,
            })),
        };
        if create {
            let _owner = store.lock_owner()?;
            inventory(store.windows()?.root.clone(), limits)?;
            store.repair_torn_tail(lease)?;
            let state = store.load()?;
            if !state.effects.values().all(|e| e.state.is_terminal())
                && !store.has_pending_reserve(&state)?
            {
                store.seal_generation(lease, &state, true)?;
            }
            store.recover_interrupted(lease)?;
        }
        Ok(Some(store))
    }
    pub fn inspect_open_at_state_dir(
        project: &Path,
        state: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementEffectInspectOpenV1> {
        Self::inspect_open_with_read_lease(
            project,
            state,
            crate::project_mutation::ProjectReadLeaseV1::Mutation(lease),
        )
    }
    pub(crate) fn inspect_open_with_read_lease(
        project: &Path,
        state: &Path,
        lease: crate::project_mutation::ProjectReadLeaseV1<'_>,
    ) -> Result<ManagementEffectInspectOpenV1> {
        let lease = lease
            .mutation()
            .map_err(|_| ManagementEffectStoreError::WrongProjectLease)?;
        let project_pin = WindowsPinnedDirectory::open_existing(project).map_err(error)?;
        let project = fs::canonicalize(project)?;
        let scope = ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes());
        let Some(store) = Self::windows_open(
            state,
            scope,
            lease,
            ManagementEffectLimitsV1::default(),
            false,
            Some(project_pin),
        )?
        else {
            return Ok(ManagementEffectInspectOpenV1::Absent);
        };
        let root = store.windows()?.root.clone();
        let owner = pin(root.clone(), OWNER_LOCK_NAME, OWNER_LOCK_NAME, 4096)?;
        owner.file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                std::io::Error::from(std::io::ErrorKind::WouldBlock)
            }
            std::fs::TryLockError::Error(e) => e,
        })?;
        let observed = inventory(root.clone(), store.limits)?;
        Self::inspect_inventory(
            store,
            Some(scope),
            root,
            owner,
            observed,
            MigrationInspectionV2::default(),
        )
    }
    pub(super) fn check_runtime_request(
        &self,
        lease: JournalLeaseV1<'_>,
        request: &PrepareManagementEffectV1,
    ) -> Result<()> {
        if matches!(lease, JournalLeaseV1::Runtime(_)) {
            return Err(unsupported());
        }
        require_inventory(request)
    }
    pub(super) fn check_journal_lease(&self, lease: JournalLeaseV1<'_>) -> Result<()> {
        let JournalLeaseV1::Mutation(lease) = lease else {
            return Err(unsupported());
        };
        self.check_lease(lease)
    }
    pub(super) fn check_lease(&self, lease: &ProjectMutationLeaseV1) -> Result<()> {
        if lease.store_root() != self.expected_lease_root {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        crate::project_mutation::ProjectReadLeaseV1::Mutation(lease)
            .verify(&self.state_dir)
            .map_err(|_| ManagementEffectStoreError::WrongProjectLease)?;
        self.windows()?.directory(&self.root).map(|_| ())
    }
    pub(super) fn lock_owner(&self) -> Result<WindowsFamilyFile> {
        let file = self
            .windows()?
            .open(&self.root.join(OWNER_LOCK_NAME), true)?;
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                std::io::Error::from(std::io::ErrorKind::WouldBlock)
            }
            std::fs::TryLockError::Error(e) => e,
        })?;
        Ok(file)
    }
}
impl ManagementEffectInspectV1 {
    pub fn root_identity(&self) -> Result<(u64, u64)> {
        self.root_directory.legacy_identity().map_err(error)
    }
    pub(super) fn verify_pins(&self) -> Result<()> {
        let named = self.store.windows()?.directory(&self.store.root)?;
        if named.identity() != self.root_directory.identity() {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        self.root_directory.revalidate().map_err(error)?;
        for pin in self.files.values() {
            verify_file(pin)?;
        }
        let fresh = inventory(self.root_directory.clone(), self.store.limits)?;
        if fresh.files.len() != self.files.len()
            || fresh.directories != self.directories
            || fresh.windows_directories.len() != self.windows_directories.len()
            || fresh.windows_directories.iter().any(|(name, directory)| {
                self.windows_directories
                    .get(name)
                    .is_none_or(|old| old.identity() != directory.identity())
            })
            || fresh.files.iter().any(|(name, file)| {
                self.files.get(name).is_none_or(|old| {
                    old.version != file.version
                        || old.bytes != file.bytes
                        || old.evidence != file.evidence
                })
            })
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        for directory in self.windows_directories.values() {
            directory.revalidate().map_err(error)?;
        }
        Ok(())
    }
}

impl ManagementEffectStoreV1 {
    pub(super) fn w_open(&self, path: &Path, write: bool) -> Result<WindowsFamilyFile> {
        self.windows()?.open(path, write)
    }
    pub(super) fn w_read(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        self.windows()?.read(path, max)
    }
    pub(super) fn w_directory(&self, path: &Path) -> Result<()> {
        self.windows()?.directory(path).map(|_| ())
    }
    pub(super) fn w_ensure(&self, path: &Path) -> Result<()> {
        self.windows()?.ensure_directory(path)
    }
    pub(super) fn w_sync(&self, path: &Path) -> Result<()> {
        self.windows()?.sync(path)
    }
    pub(super) fn w_write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.windows()?.write_new(path, bytes)
    }
    pub(super) fn w_present(&self, path: &Path) -> Result<bool> {
        self.windows()?.directory_present(path)
    }
    pub(super) fn w_rename(&self, source: &Path, target: &Path) -> Result<()> {
        self.windows()?.rename_directory(source, target)
    }
    pub(super) fn w_replace(&self, source: &Path, target: &Path) -> Result<()> {
        self.windows()?.replace(source, target)
    }
}
