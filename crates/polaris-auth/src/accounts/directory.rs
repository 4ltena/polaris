//! Fixed account-directory ownership and its non-secret protection journal.
//!
//! This module deliberately does not define the account registry document.  It
//! establishes the directory, stable locks, and the inode journal which 2b
//! must use before it writes a registry byte.

#[cfg(any(unix, test))]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::CString;
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AuthError;
#[cfg(any(unix, test))]
use crate::protection::ProtectedIdentity;

const DIRECTORY: &str = "accounts";
const POLARIS: &str = ".polaris";
#[cfg(unix)]
const OWNER_LOCK: &str = "owner.lock";
#[cfg(unix)]
const JOURNAL: &str = "protected-inodes.json";
#[cfg(unix)]
const JOURNAL_TMP: &str = "protected-inodes.json.tmp";
#[cfg(any(unix, test))]
const MAX_IDENTITIES: usize = 65_536;
#[cfg(unix)]
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(unix)]
const SLOT_COUNT: usize = 16;
#[cfg(unix)]
const REGISTRY: &str = "registry.json";
#[cfg(unix)]
const REGISTRY_TMP: &str = "registry.json.tmp";
#[cfg(unix)]
const MAX_REGISTRY_BODY_BYTES: u64 = 1024 * 1024;

/// Stable account directory rooted below one trusted, OS-resolved home.
pub struct AccountDirectory {
    #[cfg(unix)]
    home: File,
    #[cfg(unix)]
    directory: File,
    home_path: PathBuf,
}

/// One slot's refresh or login lease, released on drop. It is taken before
/// the owner lock and never replaced by PID or time.
#[cfg(unix)]
pub(crate) struct SlotLease(File);

#[cfg(not(unix))]
pub(crate) struct SlotLease {
    _private: (),
}

#[cfg(unix)]
impl Drop for SlotLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// An owner lock.  The process-local protection mutex is always acquired only
/// after this guard is held.
#[cfg(unix)]
pub struct OwnerLock(File);

#[cfg(not(unix))]
pub struct OwnerLock {
    _private: (),
}

#[cfg(unix)]
impl Drop for OwnerLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(unix)]
impl OwnerLock {
    fn verify(&self, directory: &File) -> Result<(), AuthError> {
        let current = open_at(directory, OWNER_LOCK, libc::O_RDWR)?;
        private_file(&current)?;
        let held = self.0.metadata()?;
        let current = current.metadata()?;
        if (held.dev(), held.ino()) != (current.dev(), current.ino()) {
            return Err(AuthError::Protection);
        }
        Ok(())
    }
}

/// A newly-created descriptor whose identity is durable in the non-secret
/// journal. Test-only: the product writer is `RegistryWriter`.
#[cfg(all(test, unix))]
pub struct ProtectedTemp {
    file: File,
    _owner: OwnerLock,
}

#[cfg(all(test, unix))]
impl ProtectedTemp {
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub fn sync_all(&self) -> Result<(), AuthError> {
        self.file.sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalIdentity {
    dev: u64,
    ino: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    previous_revision: u64,
    revision: u64,
    identities: Vec<JournalIdentity>,
}

#[cfg(any(unix, test))]
impl Journal {
    fn initial() -> Self {
        Self {
            version: 1,
            previous_revision: 0,
            revision: 1,
            identities: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), AuthError> {
        if self.version != 1 || self.revision == 0 || self.identities.len() > MAX_IDENTITIES {
            return Err(AuthError::RecoveryRequired);
        }
        if (self.previous_revision == 0 && (self.revision != 1 || !self.identities.is_empty()))
            || (self.previous_revision != 0
                && self.revision
                    != self
                        .previous_revision
                        .checked_add(1)
                        .ok_or(AuthError::RecoveryRequired)?)
        {
            return Err(AuthError::RecoveryRequired);
        }
        let mut previous = None;
        for identity in &self.identities {
            if identity.dev == 0 || identity.ino == 0 || previous >= Some(*identity) {
                return Err(AuthError::RecoveryRequired);
            }
            previous = Some(*identity);
        }
        Ok(())
    }

    fn contains(&self, identity: ProtectedIdentity) -> bool {
        self.identities
            .binary_search(&JournalIdentity {
                dev: identity.dev,
                ino: identity.ino,
            })
            .is_ok()
    }

    fn insert(&mut self, identity: ProtectedIdentity) -> Result<bool, AuthError> {
        let identity = JournalIdentity {
            dev: identity.dev,
            ino: identity.ino,
        };
        match self.identities.binary_search(&identity) {
            Ok(_) => Ok(false),
            Err(index) => {
                if self.identities.len() >= MAX_IDENTITIES {
                    return Err(AuthError::Protection);
                }
                self.identities.insert(index, identity);
                Ok(true)
            }
        }
    }

    fn can_prepare_tmp(&self) -> bool {
        self.identities.len() < MAX_IDENTITIES && self.revision < u64::MAX
    }

    fn protected_identities(&self) -> BTreeSet<ProtectedIdentity> {
        self.identities
            .iter()
            .map(|identity| ProtectedIdentity {
                dev: identity.dev,
                ino: identity.ino,
            })
            .collect()
    }
}

#[cfg(unix)]
fn owner() -> u32 {
    // SAFETY: `geteuid` has no arguments or memory effects.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn private_directory(file: &File) -> Result<(), AuthError> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != owner() || metadata.mode() & 0o7777 != 0o700 {
        return Err(AuthError::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn trusted_ancestor_directory(file: &File) -> Result<(), AuthError> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != owner() || metadata.mode() & 0o022 != 0 {
        return Err(AuthError::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn private_file(file: &File) -> Result<(), AuthError> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner()
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(AuthError::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn component(name: &str) -> Result<CString, AuthError> {
    if name.is_empty() || name.contains('/') {
        return Err(AuthError::Protection);
    }
    CString::new(name).map_err(|_| AuthError::Protection)
}

#[cfg(unix)]
fn open_at(parent: &File, name: &str, flags: i32) -> Result<File, AuthError> {
    let name = component(name)?;
    // SAFETY: the parent FD is live and the component is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(AuthError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn mkdir_at(parent: &File, name: &str) -> Result<(), AuthError> {
    let name = component(name)?;
    // SAFETY: the parent FD is live and the component is NUL-terminated.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(AuthError::Io(error));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn path_is_physical_home(home: &Path) -> Result<(), AuthError> {
    if !home.is_absolute()
        || home
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        || home.canonicalize().map_err(|_| AuthError::Protection)? != home
    {
        return Err(AuthError::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn open_physical_directory(home: &Path) -> Result<File, AuthError> {
    path_is_physical_home(home)?;
    let mut directory = File::open("/")?;
    for part in home.components() {
        if let Component::Normal(name) = part {
            let name = name.to_str().ok_or(AuthError::Protection)?;
            directory = open_at(&directory, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        }
    }
    trusted_ancestor_directory(&directory)?;
    Ok(directory)
}

#[cfg(unix)]
fn lock_file(directory: &File, name: &str) -> Result<File, AuthError> {
    let file = open_at(directory, name, libc::O_RDWR | libc::O_CREAT)?;
    private_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn acquire(file: File) -> Result<OwnerLock, AuthError> {
    match file.try_lock() {
        Ok(()) => Ok(OwnerLock(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(AuthError::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(AuthError::Io(error)),
    }
}

impl AccountDirectory {
    /// Creates only fixed, private metadata.  It never reads a credential or
    /// initializes the 2b registry document.
    #[cfg(unix)]
    pub fn open() -> Result<Self, AuthError> {
        let home = crate::protection::trusted_home()?;
        Self::open_trusted(&home)
    }

    #[cfg(unix)]
    fn open_trusted(home: &Path) -> Result<Self, AuthError> {
        let root = open_physical_directory(home)?;
        mkdir_at(&root, POLARIS)?;
        let polaris = open_at(&root, POLARIS, libc::O_RDONLY | libc::O_DIRECTORY)?;
        trusted_ancestor_directory(&polaris)?;
        mkdir_at(&polaris, DIRECTORY)?;
        let directory = open_at(&polaris, DIRECTORY, libc::O_RDONLY | libc::O_DIRECTORY)?;
        private_directory(&directory)?;
        root.sync_all()?;
        polaris.sync_all()?;
        let result = Self {
            home: root,
            directory,
            home_path: home.to_owned(),
        };
        let owner = result.owner_lock_for_genesis()?;
        result.ensure_fixed_files()?;
        result.ensure_journal()?;
        drop(owner);
        Ok(result)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn open_for_test(home: &Path) -> Result<Self, AuthError> {
        Self::open_trusted(home)
    }

    /// Opens an already-complete directory without creating a path or lock.
    #[cfg(unix)]
    fn open_reader(home: &Path) -> Result<Option<Self>, AuthError> {
        let root = open_physical_directory(home)?;
        let polaris = match open_at(&root, POLARIS, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(file) => file,
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(_) => return Err(AuthError::Protection),
        };
        trusted_ancestor_directory(&polaris)?;
        let directory = match open_at(&polaris, DIRECTORY, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(file) => file,
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(_) => return Err(AuthError::Protection),
        };
        private_directory(&directory)?;
        Ok(Some(Self {
            home: root,
            directory,
            home_path: home.to_owned(),
        }))
    }

    #[cfg(unix)]
    fn check_directory(&self) -> Result<(), AuthError> {
        let polaris = open_at(&self.home, POLARIS, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| AuthError::Protection)?;
        trusted_ancestor_directory(&polaris)?;
        let current = open_at(&polaris, DIRECTORY, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| AuthError::Protection)?;
        private_directory(&current)?;
        let expected = self.directory.metadata()?;
        let actual = current.metadata()?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            return Err(AuthError::Protection);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn owner_lock(&self) -> Result<OwnerLock, AuthError> {
        let lock = open_at(&self.directory, OWNER_LOCK, libc::O_RDWR)?;
        private_file(&lock)?;
        let lock = acquire(lock)?;
        self.check_directory()?;
        lock.verify(&self.directory)?;
        Ok(lock)
    }

    #[cfg(unix)]
    fn owner_lock_for_genesis(&self) -> Result<OwnerLock, AuthError> {
        if self.present(OWNER_LOCK)? {
            return self.owner_lock();
        }
        for name in [JOURNAL, JOURNAL_TMP, "registry.json", "registry.json.tmp"] {
            if self.present(name)? {
                return Err(AuthError::RecoveryRequired);
            }
        }
        for slot in 0..SLOT_COUNT {
            if self.present(&format!("refresh-{slot:02}.lock"))? {
                return Err(AuthError::RecoveryRequired);
            }
        }
        let lock = lock_file(&self.directory, OWNER_LOCK)?;
        let lock = acquire(lock)?;
        self.check_directory()?;
        lock.verify(&self.directory)?;
        Ok(lock)
    }

    #[cfg(unix)]
    fn reader_owner_lock(&self) -> Result<OwnerLock, AuthError> {
        let lock = open_at(&self.directory, OWNER_LOCK, libc::O_RDWR)?;
        private_file(&lock)?;
        let lock = acquire(lock)?;
        self.check_directory()?;
        lock.verify(&self.directory)?;
        Ok(lock)
    }

    #[cfg(unix)]
    fn ensure_fixed_files(&self) -> Result<(), AuthError> {
        let owner_lock = open_at(&self.directory, OWNER_LOCK, libc::O_RDWR)?;
        private_file(&owner_lock)?;
        owner_lock.sync_all()?;
        let initializing = !self.present(JOURNAL)?
            && !self.present(JOURNAL_TMP)?
            && !self.present("registry.json")?
            && !self.present("registry.json.tmp")?;
        for slot in 0..SLOT_COUNT {
            let name = format!("refresh-{slot:02}.lock");
            let lock = if initializing {
                lock_file(&self.directory, &name)?
            } else {
                open_at(&self.directory, &name, libc::O_RDWR)?
            };
            private_file(&lock)?;
            lock.sync_all()?;
        }
        self.directory.sync_all()?;
        Ok(())
    }

    #[cfg(unix)]
    fn journal_file(&self, name: &str) -> Result<File, AuthError> {
        let file = open_at(&self.directory, name, libc::O_RDONLY)?;
        private_file(&file)?;
        Ok(file)
    }

    #[cfg(unix)]
    fn read_journal(&self, name: &str) -> Result<Journal, AuthError> {
        let file = self.journal_file(name)?;
        let length = file.metadata()?.len();
        if length > MAX_JOURNAL_BYTES {
            return Err(AuthError::RecoveryRequired);
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(AuthError::RecoveryRequired);
        }
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| AuthError::RecoveryRequired)?;
        journal.validate()?;
        Ok(journal)
    }

    #[cfg(unix)]
    fn present(&self, name: &str) -> Result<bool, AuthError> {
        match open_at(&self.directory, name, libc::O_RDONLY) {
            Ok(file) => {
                private_file(&file)?;
                Ok(true)
            }
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(AuthError::Protection),
        }
    }

    #[cfg(unix)]
    fn publish_journal(&self, journal: &Journal, canonical_absent: bool) -> Result<(), AuthError> {
        let body = serde_json::to_vec(journal).map_err(|_| AuthError::RecoveryRequired)?;
        if body.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(AuthError::Protection);
        }
        if self.present(JOURNAL_TMP)? {
            return Err(AuthError::RecoveryRequired);
        }
        let mut tmp = open_at(
            &self.directory,
            JOURNAL_TMP,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        private_file(&tmp)?;
        tmp.write_all(&body)?;
        tmp.sync_all()?;
        let tmp_journal = self.read_journal(JOURNAL_TMP)?;
        if tmp_journal.revision != journal.revision || tmp_journal.identities != journal.identities
        {
            return Err(AuthError::RecoveryRequired);
        }
        if !canonical_absent && !self.present(JOURNAL)? {
            return Err(AuthError::RecoveryRequired);
        }
        let old = component(JOURNAL_TMP)?;
        let new = component(JOURNAL)?;
        // SAFETY: both names are fixed components within the held directory.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                old.as_ptr(),
                self.directory.as_raw_fd(),
                new.as_ptr(),
            )
        } != 0
        {
            return Err(AuthError::Io(std::io::Error::last_os_error()));
        }
        self.directory.sync_all()?;
        Ok(())
    }

    #[cfg(unix)]
    fn ensure_journal(&self) -> Result<Journal, AuthError> {
        let canonical = self.present(JOURNAL)?;
        let temporary = self.present(JOURNAL_TMP)?;
        match (canonical, temporary) {
            (false, false) => {
                let initial = Journal::initial();
                self.publish_journal(&initial, true)?;
                Ok(initial)
            }
            (false, true) => {
                let journal = self.read_journal(JOURNAL_TMP)?;
                if journal != Journal::initial() {
                    return Err(AuthError::RecoveryRequired);
                }
                let old = component(JOURNAL_TMP)?;
                let new = component(JOURNAL)?;
                // SAFETY: fixed names and an owned directory descriptor.
                if unsafe {
                    libc::renameat(
                        self.directory.as_raw_fd(),
                        old.as_ptr(),
                        self.directory.as_raw_fd(),
                        new.as_ptr(),
                    )
                } != 0
                {
                    return Err(AuthError::Io(std::io::Error::last_os_error()));
                }
                self.directory.sync_all()?;
                Ok(journal)
            }
            (true, false) => self.read_journal(JOURNAL),
            (true, true) => {
                let canonical = self.read_journal(JOURNAL)?;
                let temporary = self.read_journal(JOURNAL_TMP)?;
                if temporary.revision
                    != canonical
                        .revision
                        .checked_add(1)
                        .ok_or(AuthError::RecoveryRequired)?
                    || temporary.identities.len() < canonical.identities.len()
                    || !canonical
                        .identities
                        .iter()
                        .all(|identity| temporary.identities.binary_search(identity).is_ok())
                {
                    return Err(AuthError::RecoveryRequired);
                }
                let old = component(JOURNAL_TMP)?;
                let new = component(JOURNAL)?;
                // SAFETY: fixed names and an owned directory descriptor.
                if unsafe {
                    libc::renameat(
                        self.directory.as_raw_fd(),
                        old.as_ptr(),
                        self.directory.as_raw_fd(),
                        new.as_ptr(),
                    )
                } != 0
                {
                    return Err(AuthError::Io(std::io::Error::last_os_error()));
                }
                self.directory.sync_all()?;
                Ok(temporary)
            }
        }
    }

    /// Allocate the one registry tmp descriptor used by 2b.  The identity is
    /// durably journaled before this returns, so callers may then write secret
    /// bytes.  Existing tmp files always require explicit recovery.
    #[cfg(all(test, unix))]
    pub(crate) fn prepare_registry_tmp(&self) -> Result<ProtectedTemp, AuthError> {
        let owner = self.owner_lock()?;
        let mut journal = self.ensure_journal()?;
        if self.present("registry.json.tmp")? {
            return Err(AuthError::RecoveryRequired);
        }
        // A newly allocated descriptor has no safe claim to an old identity.
        // Reject before O_EXCL so a full journal cannot leave an unrecorded tmp.
        if !journal.can_prepare_tmp() {
            return Err(AuthError::Protection);
        }
        let file = open_at(
            &self.directory,
            "registry.json.tmp",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )?;
        private_file(&file)?;
        let metadata = file.metadata()?;
        let identity = ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        if journal.insert(identity)? {
            let previous = journal.revision;
            journal.revision = journal
                .revision
                .checked_add(1)
                .ok_or(AuthError::Protection)?;
            journal.previous_revision = previous;
            self.publish_journal(&journal, false)?;
        }
        crate::protection::merge_account_identities(&journal.protected_identities())?;
        Ok(ProtectedTemp {
            file,
            _owner: owner,
        })
    }

    /// Atomically publish the prepared registry descriptor.  The retained
    /// owner lock keeps the journal identity and this rename in one writer
    /// critical section.
    #[cfg(all(test, unix))]
    pub(crate) fn publish_registry_tmp(&self, temp: ProtectedTemp) -> Result<(), AuthError> {
        temp._owner.verify(&self.directory)?;
        self.check_directory()?;
        private_file(&temp.file)?;
        let metadata = temp.file.metadata()?;
        let identity = ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        let journal = self.read_journal(JOURNAL)?;
        if !journal.contains(identity) {
            return Err(AuthError::RecoveryRequired);
        }
        temp.file.sync_all()?;
        let current = open_at(&self.directory, "registry.json.tmp", libc::O_RDWR)?;
        private_file(&current)?;
        let current = current.metadata()?;
        if (current.dev(), current.ino()) != (identity.dev, identity.ino) {
            return Err(AuthError::Protection);
        }
        let old = component("registry.json.tmp")?;
        let new = component("registry.json")?;
        // SAFETY: both names are fixed components in the retained directory.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                old.as_ptr(),
                self.directory.as_raw_fd(),
                new.as_ptr(),
            )
        } != 0
        {
            return Err(AuthError::Io(std::io::Error::last_os_error()));
        }
        self.directory.sync_all()?;
        temp._owner.verify(&self.directory)?;
        Ok(())
    }

    /// Mutable owner: completes the journal, then reads both registry names.
    #[cfg(unix)]
    pub(crate) fn registry_writer(&self) -> Result<RegistryWriter<'_>, AuthError> {
        let owner = self.owner_lock()?;
        let journal = self.ensure_journal()?;
        let canonical = self
            .read_journaled(REGISTRY, &journal)?
            .map(|(body, _)| body);
        let temporary = self.read_journaled(REGISTRY_TMP, &journal)?;
        owner.verify(&self.directory)?;
        Ok(RegistryWriter {
            directory: self,
            _owner: owner,
            journal,
            canonical,
            temporary,
        })
    }

    /// A registry name is readable only if its inode is journaled.
    #[cfg(unix)]
    fn read_journaled(
        &self,
        name: &str,
        journal: &Journal,
    ) -> Result<Option<(Vec<u8>, File)>, AuthError> {
        let file = match open_at(&self.directory, name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(_) => return Err(AuthError::Protection),
        };
        private_file(&file)?;
        let metadata = file.metadata()?;
        if !journal.contains(ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }) || metadata.len() > MAX_REGISTRY_BODY_BYTES
        {
            return Err(AuthError::RecoveryRequired);
        }
        let mut body = Vec::with_capacity(metadata.len() as usize);
        (&file)
            .take(MAX_REGISTRY_BODY_BYTES + 1)
            .read_to_end(&mut body)?;
        if body.len() as u64 > MAX_REGISTRY_BODY_BYTES {
            return Err(AuthError::RecoveryRequired);
        }
        Ok(Some((body, file)))
    }

    /// The communication lease of one slot, or `None` while another owner holds it.
    #[cfg(unix)]
    pub(crate) fn try_slot_lease(&self, index: usize) -> Result<Option<SlotLease>, AuthError> {
        if index >= SLOT_COUNT {
            return Err(AuthError::AccountLimit);
        }
        let file = open_at(
            &self.directory,
            &format!("refresh-{index:02}.lock"),
            libc::O_RDWR,
        )?;
        private_file(&file)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(SlotLease(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(AuthError::Io(error)),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.home_path.join(POLARIS).join(DIRECTORY)
    }
}

/// Read-only callers: creates no directory, lock or journal and never
/// recovers. Any tmp or incomplete infrastructure is `RecoveryRequired`.
#[cfg(unix)]
pub(crate) fn read_registry_read_only(home: &Path) -> Result<Option<Vec<u8>>, AuthError> {
    let Some(directory) = AccountDirectory::open_reader(home)? else {
        return Ok(None);
    };
    if !directory.present(OWNER_LOCK)?
        || !directory.present(JOURNAL)?
        || directory.present(JOURNAL_TMP)?
    {
        return Err(AuthError::RecoveryRequired);
    }
    let owner = directory.reader_owner_lock()?;
    let journal = directory.read_journal(JOURNAL)?;
    if directory.present(REGISTRY_TMP)? {
        return Err(AuthError::RecoveryRequired);
    }
    let body = directory
        .read_journaled(REGISTRY, &journal)?
        .map(|(body, _)| body);
    owner.verify(&directory.directory)?;
    Ok(body)
}

/// One owner-lock critical section over the registry names. Recovery, the
/// transition check and the replacement all run before this lock is dropped.
pub(crate) struct RegistryWriter<'a> {
    #[cfg(unix)]
    directory: &'a AccountDirectory,
    #[cfg(not(unix))]
    _directory: &'a AccountDirectory,
    _owner: OwnerLock,
    journal: Journal,
    canonical: Option<Vec<u8>>,
    temporary: Option<(Vec<u8>, File)>,
}

#[cfg(all(test, unix))]
thread_local! {
    pub(crate) static REGISTRY_FAULT: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, unix))]
fn fault(point: &'static str) -> Result<(), AuthError> {
    if REGISTRY_FAULT.with(|f| f.get()) == Some(point) {
        return Err(AuthError::Io(std::io::Error::other(point)));
    }
    Ok(())
}

impl RegistryWriter<'_> {
    pub(crate) fn canonical(&self) -> Option<&[u8]> {
        self.canonical.as_deref()
    }

    pub(crate) fn temporary(&self) -> Option<&[u8]> {
        self.temporary.as_ref().map(|(body, _)| body.as_slice())
    }

    pub(crate) fn protection_revision(&self) -> u64 {
        self.journal.revision
    }

    /// Publishes the complete tmp the store has already matched to canonical.
    #[cfg(unix)]
    pub(crate) fn publish_temporary(&mut self) -> Result<(), AuthError> {
        let (body, file) = self.temporary.take().ok_or(AuthError::RecoveryRequired)?;
        file.sync_all()?;
        self.rename_registry_tmp(&file)?;
        self.canonical = Some(body);
        Ok(())
    }

    /// Creates a new tmp, journals its inode before any byte, then writes the
    /// body built for that journal revision, syncs and publishes it.
    #[cfg(unix)]
    pub(crate) fn replace(
        &mut self,
        build: impl FnOnce(u64) -> Result<Vec<u8>, AuthError>,
    ) -> Result<(), AuthError> {
        if self.temporary.is_some() || !self.journal.can_prepare_tmp() {
            return Err(AuthError::RecoveryRequired);
        }
        let mut file = open_at(
            &self.directory.directory,
            REGISTRY_TMP,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )?;
        private_file(&file)?;
        let metadata = file.metadata()?;
        if self.journal.insert(ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })? {
            let previous = self.journal.revision;
            self.journal.revision = previous.checked_add(1).ok_or(AuthError::Protection)?;
            self.journal.previous_revision = previous;
            self.directory.publish_journal(&self.journal, false)?;
        }
        crate::protection::merge_account_identities(&self.journal.protected_identities())?;
        #[cfg(all(test, unix))]
        fault("registry_tmp_created")?;
        let body = build(self.journal.revision)?;
        if body.len() as u64 > MAX_REGISTRY_BODY_BYTES {
            return Err(AuthError::Protection);
        }
        #[cfg(all(test, unix))]
        if REGISTRY_FAULT.with(|f| f.get()) == Some("registry_tmp_partial") {
            file.write_all(&body[..body.len() / 2])?;
            file.sync_all()?;
            return Err(AuthError::Io(std::io::Error::other("registry_tmp_partial")));
        }
        file.write_all(&body)?;
        file.sync_all()?;
        #[cfg(all(test, unix))]
        fault("registry_tmp_synced")?;
        self.rename_registry_tmp(&file)?;
        self.canonical = Some(body);
        Ok(())
    }

    #[cfg(unix)]
    fn rename_registry_tmp(&self, file: &File) -> Result<(), AuthError> {
        self._owner.verify(&self.directory.directory)?;
        self.directory.check_directory()?;
        private_file(file)?;
        let held = file.metadata()?;
        if !self.journal.contains(ProtectedIdentity {
            dev: held.dev(),
            ino: held.ino(),
        }) {
            return Err(AuthError::RecoveryRequired);
        }
        let named = open_at(&self.directory.directory, REGISTRY_TMP, libc::O_RDONLY)?;
        private_file(&named)?;
        let named = named.metadata()?;
        if (named.dev(), named.ino()) != (held.dev(), held.ino()) {
            return Err(AuthError::Protection);
        }
        rename_fixed(&self.directory.directory, REGISTRY_TMP, REGISTRY)?;
        self.directory.directory.sync_all()?;
        Ok(())
    }
}

#[cfg(unix)]
fn rename_fixed(directory: &File, from: &str, to: &str) -> Result<(), AuthError> {
    let old = component(from)?;
    let new = component(to)?;
    // SAFETY: both names are fixed components in the retained directory.
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            old.as_ptr(),
            directory.as_raw_fd(),
            new.as_ptr(),
        )
    } != 0
    {
        return Err(AuthError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Hold the accounts owner lock through a non-secret reader copy.  An absent
/// directory uses the old path only if it is still absent after the callback.
#[cfg(unix)]
pub(crate) fn with_reader_protection<R>(
    home: &Path,
    callback: impl FnOnce(&crate::protection::ProtectedPathsSnapshot) -> R,
) -> Result<R, AuthError> {
    let Some(directory) = AccountDirectory::open_reader(home)? else {
        let value = crate::protection::with_account_identities(&BTreeSet::new(), callback)?;
        if AccountDirectory::open_reader(home)?.is_some() {
            return Err(AuthError::RecoveryRequired);
        }
        return Ok(value);
    };
    if !directory.present(OWNER_LOCK)? || !directory.present(JOURNAL)? {
        return Err(AuthError::RecoveryRequired);
    }
    if directory.present(JOURNAL_TMP)? {
        return Err(AuthError::RecoveryRequired);
    }
    let _owner = directory.reader_owner_lock()?;
    let journal = directory.read_journal(JOURNAL)?;
    crate::protection::with_account_identities(&journal.protected_identities(), callback)
}

// Windows must not acquire a lease or create state without a native owner.
#[cfg(not(unix))]
impl AccountDirectory {
    pub fn open() -> Result<Self, AuthError> {
        Err(AuthError::Protection)
    }

    #[cfg(test)]
    pub(crate) fn open_for_test(_home: &Path) -> Result<Self, AuthError> {
        Err(AuthError::Protection)
    }

    pub(crate) fn registry_writer(&self) -> Result<RegistryWriter<'_>, AuthError> {
        Err(AuthError::Protection)
    }

    pub(crate) fn try_slot_lease(&self, _index: usize) -> Result<Option<SlotLease>, AuthError> {
        Err(AuthError::Protection)
    }
}

#[cfg(not(unix))]
pub(crate) fn read_registry_read_only(_home: &Path) -> Result<Option<Vec<u8>>, AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
impl RegistryWriter<'_> {
    pub(crate) fn publish_temporary(&mut self) -> Result<(), AuthError> {
        Err(AuthError::Protection)
    }

    pub(crate) fn replace(
        &mut self,
        _build: impl FnOnce(u64) -> Result<Vec<u8>, AuthError>,
    ) -> Result<(), AuthError> {
        Err(AuthError::Protection)
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    #[test]
    fn journal_validation_preserves_revision_and_identity_contract() {
        let mut journal = Journal::initial();
        assert!(journal.validate().is_ok());
        assert!(journal.can_prepare_tmp());
        let identity = ProtectedIdentity { dev: 1, ino: 2 };
        assert!(journal.insert(identity).unwrap());
        assert!(!journal.insert(identity).unwrap());
        assert!(journal.contains(identity));
        assert_eq!(journal.protected_identities(), BTreeSet::from([identity]));
        assert!(matches!(
            journal.validate(),
            Err(AuthError::RecoveryRequired)
        ));
        journal.previous_revision = 1;
        journal.revision = 2;
        assert!(journal.validate().is_ok());
        journal.revision = 3;
        assert!(matches!(
            journal.validate(),
            Err(AuthError::RecoveryRequired)
        ));
        journal.revision = 2;
        journal.identities.push(journal.identities[0]);
        assert!(matches!(
            journal.validate(),
            Err(AuthError::RecoveryRequired)
        ));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_auth_unavailable_accounts_directory_preserves_paths() {
        let root = tempfile::tempdir().unwrap();
        let account_path = root.path().join(".polaris").join("accounts");
        std::fs::create_dir_all(&account_path).unwrap();
        let names = [
            "registry.json",
            "registry.json.tmp",
            "protected-inodes.json",
            "owner.lock",
        ];
        for name in names {
            std::fs::write(account_path.join(name), b"existing fixture").unwrap();
        }
        let missing = root.path().join("missing");
        assert!(matches!(
            AccountDirectory::open(),
            Err(AuthError::Protection)
        ));
        for home in [root.path(), missing.as_path()] {
            assert!(matches!(
                AccountDirectory::open_for_test(home),
                Err(AuthError::Protection)
            ));
            assert!(matches!(
                read_registry_read_only(home),
                Err(AuthError::Protection)
            ));
        }
        for name in names {
            assert_eq!(
                std::fs::read(account_path.join(name)).unwrap(),
                b"existing fixture"
            );
        }
        assert!(!missing.exists());
    }

    #[test]
    fn windows_auth_unavailable_accounts_writer_never_invokes_builder() {
        // Private synthetic state exercises refusal even if a caller holds a
        // typed value. No native directory, lock or writer can be opened.
        let directory = AccountDirectory {
            home_path: PathBuf::from("fixture"),
        };
        assert_eq!(
            directory.path(),
            Path::new("fixture").join(".polaris/accounts")
        );
        assert!(matches!(
            directory.registry_writer(),
            Err(AuthError::Protection)
        ));
        assert!(matches!(
            directory.try_slot_lease(0),
            Err(AuthError::Protection)
        ));
        assert!(matches!(
            directory.try_slot_lease(usize::MAX),
            Err(AuthError::Protection)
        ));
        let mut writer = RegistryWriter {
            _directory: &directory,
            _owner: OwnerLock { _private: () },
            journal: Journal::initial(),
            canonical: Some(b"original fixture".to_vec()),
            temporary: None,
        };
        assert!(matches!(
            writer.replace(|_| panic!("builder called")),
            Err(AuthError::Protection)
        ));
        assert!(matches!(
            writer.publish_temporary(),
            Err(AuthError::Protection)
        ));
        assert_eq!(writer.canonical(), Some(b"original fixture".as_slice()));
        assert_eq!(writer.temporary(), None);
        assert_eq!(writer.protection_revision(), 1);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::Command;

    fn home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    fn physical(home: &tempfile::TempDir) -> PathBuf {
        home.path().canonicalize().unwrap()
    }

    #[test]
    fn genesis_creates_only_fixed_private_nonsecret_infrastructure() {
        let home = home();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        let path = directory.path();
        assert!(path.join(OWNER_LOCK).is_file());
        assert!(path.join(JOURNAL).is_file());
        for slot in 0..SLOT_COUNT {
            assert!(path.join(format!("refresh-{slot:02}.lock")).is_file());
        }
        let journal = directory.read_journal(JOURNAL).unwrap();
        assert_eq!(journal, Journal::initial());
        assert!(!path.join("registry.json").exists());
        for entry in std::fs::read_dir(path).unwrap() {
            let metadata = entry.unwrap().metadata().unwrap();
            assert_eq!(metadata.uid(), owner());
            assert_eq!(metadata.mode() & 0o7777, 0o600);
        }
    }

    #[test]
    fn owner_only_readable_ancestor_is_accepted_but_accounts_stays_exactly_private() {
        let home = home();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        assert_eq!(
            std::fs::metadata(directory.path()).unwrap().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn non_private_accounts_directory_is_rejected() {
        let home = home();
        let physical = physical(&home);
        std::fs::create_dir_all(physical.join(".polaris/accounts")).unwrap();
        std::fs::set_permissions(
            physical.join(".polaris"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            physical.join(".polaris/accounts"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        assert!(matches!(
            AccountDirectory::open_for_test(&physical),
            Err(AuthError::Protection)
        ));
    }

    #[test]
    fn protected_tmp_is_recorded_before_any_caller_write() {
        let home = home();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        let mut tmp = directory.prepare_registry_tmp().unwrap();
        let identity = tmp.file_mut().metadata().unwrap();
        let journal = directory.read_journal(JOURNAL).unwrap();
        assert!(journal.contains(ProtectedIdentity {
            dev: identity.dev(),
            ino: identity.ino()
        }));
        tmp.file_mut().write_all(b"synthetic-secret").unwrap();
        tmp.sync_all().unwrap();
        directory.publish_registry_tmp(tmp).unwrap();
        assert!(directory.path().join("registry.json").is_file());
    }

    #[test]
    fn corrupt_or_unexpected_journal_tmp_fails_closed_without_deletion() {
        let home = home();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        let tmp = directory.path().join(JOURNAL_TMP);
        std::fs::write(&tmp, b"not-json").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            directory.ensure_journal(),
            Err(AuthError::RecoveryRequired)
        ));
        assert!(directory.path().join(JOURNAL_TMP).exists());
        assert!(matches!(
            directory.prepare_registry_tmp(),
            Err(AuthError::RecoveryRequired)
        ));
        assert!(!directory.path().join("registry.json.tmp").exists());
    }

    #[test]
    fn reader_rejects_directory_without_complete_journal_and_lock() {
        let home = home();
        std::fs::create_dir_all(home.path().join(".polaris/accounts")).unwrap();
        std::fs::set_permissions(
            home.path().join(".polaris"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            home.path().join(".polaris/accounts"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(matches!(
            with_reader_protection(&physical(&home), |_| ()),
            Err(AuthError::RecoveryRequired)
        ));
        assert!(!home.path().join(".polaris/accounts/owner.lock").exists());
    }

    #[test]
    fn replacement_of_the_live_owner_lock_is_rejected_by_identity() {
        let home = home();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        let held = directory.owner_lock().unwrap();
        let path = directory.path();
        std::fs::rename(path.join(OWNER_LOCK), path.join("retired-owner.lock")).unwrap();
        std::fs::write(path.join(OWNER_LOCK), b"replacement").unwrap();
        std::fs::set_permissions(
            path.join(OWNER_LOCK),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(matches!(
            held.verify(&directory.directory),
            Err(AuthError::Protection)
        ));
    }

    #[test]
    fn hardlinked_prepared_tmp_stays_journaled_and_cannot_publish() {
        let home = home();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        let mut tmp = directory.prepare_registry_tmp().unwrap();
        let metadata = tmp.file_mut().metadata().unwrap();
        let identity = ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        tmp.file_mut().write_all(b"synthetic-secret").unwrap();
        std::fs::hard_link(
            directory.path().join("registry.json.tmp"),
            directory.path().join("escaped-hardlink"),
        )
        .unwrap();
        assert!(matches!(
            directory.publish_registry_tmp(tmp),
            Err(AuthError::Protection)
        ));
        assert!(directory.read_journal(JOURNAL).unwrap().contains(identity));
    }

    #[test]
    fn complete_initial_journal_tmp_recovers_only_after_fixed_genesis() {
        let home = home();
        let physical = physical(&home);
        let directory = AccountDirectory::open_for_test(&physical).unwrap();
        let path = directory.path();
        std::fs::remove_file(path.join(JOURNAL)).unwrap();
        std::fs::write(
            path.join(JOURNAL_TMP),
            serde_json::to_vec(&Journal::initial()).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(
            path.join(JOURNAL_TMP),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        drop(directory);
        let recovered = AccountDirectory::open_for_test(&physical).unwrap();
        assert_eq!(recovered.read_journal(JOURNAL).unwrap(), Journal::initial());
        assert!(!recovered.path().join(JOURNAL_TMP).exists());
    }

    #[test]
    fn cross_process_owner_lock_reports_busy() {
        if let Some(path) = std::env::var_os("POLARIS_ACCOUNT_LOCK_TEST") {
            let file = open_at(
                &File::open(Path::new(&path)).unwrap(),
                OWNER_LOCK,
                libc::O_RDWR,
            )
            .unwrap();
            assert!(matches!(
                file.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            return;
        }
        let home = home();
        let directory = AccountDirectory::open_for_test(&physical(&home)).unwrap();
        let _held = directory.owner_lock().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "accounts::directory::tests::cross_process_owner_lock_reports_busy",
            ])
            .env("POLARIS_ACCOUNT_LOCK_TEST", directory.path())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn fresh_reader_imports_journal_or_discards_absent_snapshot() {
        if let Some(kind) = std::env::var_os("POLARIS_ACCOUNT_READER_TEST") {
            let home = PathBuf::from(std::env::var_os("POLARIS_ACCOUNT_READER_HOME").unwrap());
            crate::protection::use_trusted_home(&home).unwrap();
            if kind == "identity" {
                let expected = std::env::var("POLARIS_ACCOUNT_READER_ID").unwrap();
                let (dev, ino) = expected.split_once(':').unwrap();
                let expected = ProtectedIdentity {
                    dev: dev.parse().unwrap(),
                    ino: ino.parse().unwrap(),
                };
                with_reader_protection(&home, |snapshot| {
                    assert!(snapshot.identities().contains(&expected));
                })
                .unwrap();
            } else {
                assert!(matches!(
                    with_reader_protection(&home, |_| {
                        AccountDirectory::open_for_test(&home).unwrap();
                    }),
                    Err(AuthError::RecoveryRequired)
                ));
            }
            return;
        }

        let home = home();
        let physical = physical(&home);
        let directory = AccountDirectory::open_for_test(&physical).unwrap();
        let mut temp = directory.prepare_registry_tmp().unwrap();
        let metadata = temp.file_mut().metadata().unwrap();
        temp.file_mut().write_all(b"synthetic-secret").unwrap();
        directory.publish_registry_tmp(temp).unwrap();
        let id = format!("{}:{}", metadata.dev(), metadata.ino());
        for kind in ["identity", "absence"] {
            let absent = kind == "absence";
            let child_home = if absent {
                let isolated = tempfile::tempdir().unwrap();
                std::fs::set_permissions(isolated.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                let path = isolated.path().canonicalize().unwrap();
                let status = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "accounts::directory::tests::fresh_reader_imports_journal_or_discards_absent_snapshot"])
                    .env("POLARIS_ACCOUNT_READER_TEST", kind)
                    .env("POLARIS_ACCOUNT_READER_HOME", &path)
                    .status()
                    .unwrap();
                assert!(status.success());
                continue;
            } else {
                physical.clone()
            };
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "accounts::directory::tests::fresh_reader_imports_journal_or_discards_absent_snapshot"])
                .env("POLARIS_ACCOUNT_READER_TEST", kind)
                .env("POLARIS_ACCOUNT_READER_HOME", &child_home)
                .env("POLARIS_ACCOUNT_READER_ID", &id)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    #[test]
    fn genesis_resumes_after_lock_or_journal_publish_only() {
        let home = home();
        let physical = physical(&home);
        std::fs::create_dir_all(physical.join(".polaris/accounts")).unwrap();
        std::fs::set_permissions(
            physical.join(".polaris"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(
            physical.join(".polaris/accounts"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let accounts = physical.join(".polaris/accounts");
        std::fs::write(accounts.join(OWNER_LOCK), b"").unwrap();
        std::fs::set_permissions(
            accounts.join(OWNER_LOCK),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let first = AccountDirectory::open_for_test(&physical).unwrap();
        assert_eq!(first.read_journal(JOURNAL).unwrap(), Journal::initial());
        drop(first);
        let second = AccountDirectory::open_for_test(&physical).unwrap();
        assert_eq!(second.read_journal(JOURNAL).unwrap(), Journal::initial());
    }

    #[test]
    fn symlink_or_replaced_accounts_directory_is_never_used() {
        let home = home();
        let physical = physical(&home);
        std::fs::create_dir(physical.join(".polaris")).unwrap();
        std::fs::set_permissions(
            physical.join(".polaris"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), physical.join(".polaris/accounts")).unwrap();
        assert!(matches!(
            AccountDirectory::open_for_test(&physical),
            Err(AuthError::Protection) | Err(AuthError::Io(_))
        ));

        std::fs::remove_file(physical.join(".polaris/accounts")).unwrap();
        let directory = AccountDirectory::open_for_test(&physical).unwrap();
        let path = directory.path();
        std::fs::rename(&path, physical.join(".polaris/retired-accounts")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            directory.prepare_registry_tmp(),
            Err(AuthError::Protection) | Err(AuthError::Io(_))
        ));
    }

    #[test]
    fn journal_identity_cap_rejects_before_registry_tmp_creation() {
        let home = home();
        let physical = physical(&home);
        let directory = AccountDirectory::open_for_test(&physical).unwrap();
        let journal = Journal {
            version: 1,
            previous_revision: 1,
            revision: 2,
            identities: (1..=MAX_IDENTITIES as u64)
                .map(|ino| JournalIdentity { dev: 1, ino })
                .collect(),
        };
        let path = directory.path().join(JOURNAL);
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            directory.prepare_registry_tmp(),
            Err(AuthError::Protection)
        ));
        assert!(!directory.path().join("registry.json.tmp").exists());
    }

    #[test]
    fn journal_one_identity_below_cap_allows_a_new_preflight() {
        let journal = Journal {
            version: 1,
            previous_revision: 1,
            revision: 2,
            identities: (1..MAX_IDENTITIES as u64)
                .map(|ino| JournalIdentity { dev: 1, ino })
                .collect(),
        };
        assert!(journal.can_prepare_tmp());
    }
}
