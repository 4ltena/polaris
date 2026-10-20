//! Descriptor-relative private storage shared by trusted host surfaces.
//!
//! A stable file lock serializes cooperating processes. The protection lock is
//! held from descriptor inspection until durable replacement; paths and retired
//! inodes remain excluded from model-visible snapshots.

pub mod documents_v2;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod ledger_v1;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) mod migration_v2;

use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Read;
#[cfg(any(unix, test))]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use polaris_auth::protection::protect_store;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::{Zeroize, Zeroizing};

use crate::Error;
#[cfg(unix)]
use crate::catalog::index::MAX_INDEX_BYTES;
use crate::catalog::index::{CatalogIndex, ServerIndex};
use crate::registration::{Destination, Registry, valid_id};
#[cfg(unix)]
use crate::registration::{MAX_STORE_BYTES, bounded_json};

#[cfg(unix)]
const CATALOG: &str = "catalog.json";
#[cfg(unix)]
static RUNTIME_PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
const RUNTIME_MAX_ENTRIES: usize = 100_000;
#[cfg(unix)]
const RUNTIME_MAX_FILES: usize = 50_000;
#[cfg(unix)]
const RUNTIME_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
#[cfg(unix)]
const RUNTIME_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(unix)]
const RUNTIME_MAX_DEPTH: usize = 64;

/// Secret values deliberately have no Debug implementation or public serializer.
pub struct Secrets(SecretDocument);

/// One credential from a trusted surface. Its in-memory bytes are wiped when
/// this value is dropped; Debug never shows the value.
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Copies valid UTF-8 secret bytes from the masked input buffer. The source
    /// buffer is then dropped and wiped. This stays crate-private so no
    /// ordinary caller can obtain a plaintext view of a secret.
    pub(crate) fn from_secret_bytes(value: Zeroizing<Vec<u8>>) -> Self {
        let value = String::from_utf8(value.as_slice().to_vec())
            .expect("SecretInputV1 only constructs valid UTF-8");
        Self(Zeroizing::new(value))
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// Both documents from one locked read. No serialization or Debug output may
/// expose credentials; absence remains explicit rather than inventing revisions.
pub struct Snapshot {
    pub registry: Option<Registry>,
    pub secrets: Option<Secrets>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretDocument {
    schema_version: u32,
    revision: u64,
    values: BTreeMap<String, StoredSecretValue>,
}

// Each successfully parsed value owns its wipe guard immediately, even when
// a later map value or required document field makes deserialization fail.
struct StoredSecretValue(Zeroizing<String>);
impl std::ops::Deref for StoredSecretValue {
    type Target = str;
    fn deref(&self) -> &str {
        self.0.as_str()
    }
}
impl PartialEq for StoredSecretValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}
impl Eq for StoredSecretValue {}
impl Serialize for StoredSecretValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}
impl<'de> Deserialize<'de> for StoredSecretValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|value| Self(Zeroizing::new(value)))
    }
}
#[cfg(test)]
thread_local! {
    static STORED_SECRET_WIPES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
impl Drop for StoredSecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        if self.0.is_empty() {
            STORED_SECRET_WIPES.with(|count| count.set(count.get() + 1));
        }
    }
}

impl Secrets {
    pub fn new(revision: u64, values: BTreeMap<String, String>) -> Result<Self, Error> {
        Self::from_stored(
            revision,
            values
                .into_iter()
                .map(|(key, value)| (key, StoredSecretValue(Zeroizing::new(value))))
                .collect(),
        )
    }
    fn from_stored(
        revision: u64,
        values: BTreeMap<String, StoredSecretValue>,
    ) -> Result<Self, Error> {
        let result = Self(SecretDocument {
            schema_version: 1,
            revision,
            values,
        });
        result.0.validate()?;
        Ok(result)
    }

    pub fn revision(&self) -> u64 {
        self.0.revision
    }

    /// Reference names only; values stay inside this type.
    pub fn references(&self) -> impl Iterator<Item = &str> {
        self.0.values.keys().map(String::as_str)
    }

    /// Only the trusted transport injector may consume this value.
    pub fn resolve(&self, reference: &str) -> Result<&str, Error> {
        self.0
            .values
            .get(reference)
            .map(|value| value.0.as_str())
            .ok_or(Error::Auth)
    }
}

trait Document: Serialize + DeserializeOwned {
    #[cfg(unix)]
    fn revision(&self) -> u64;
    fn validate(&self) -> Result<(), Error>;
    #[cfg(unix)]
    fn validate_links(
        &self,
        store: &Store,
        guard: &mut polaris_auth::protection::StoreProtectionGuard,
    ) -> Result<(), Error>;
}

impl Document for Registry {
    #[cfg(unix)]
    fn revision(&self) -> u64 {
        self.revision
    }
    fn validate(&self) -> Result<(), Error> {
        Registry::validate(self)
    }
    #[cfg(unix)]
    fn validate_links(
        &self,
        store: &Store,
        guard: &mut polaris_auth::protection::StoreProtectionGuard,
    ) -> Result<(), Error> {
        store.require_no_temp("secrets.json")?;
        let secrets: Option<SecretDocument> = store.read("secrets.json", guard)?;
        check_references(self, secrets.as_ref())
    }
}

impl Document for SecretDocument {
    #[cfg(unix)]
    fn revision(&self) -> u64 {
        self.revision
    }
    fn validate(&self) -> Result<(), Error> {
        if self.schema_version != 1
            || self.revision == 0
            || self.values.iter().any(|(key, value)| {
                !valid_id(key)
                    || value.is_empty()
                    || value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
            })
        {
            return Err(Error::Auth);
        }
        Ok(())
    }
    #[cfg(unix)]
    fn validate_links(
        &self,
        store: &Store,
        guard: &mut polaris_auth::protection::StoreProtectionGuard,
    ) -> Result<(), Error> {
        store.require_no_temp("servers.json")?;
        if let Some(registry) = store.read::<Registry>("servers.json", guard)? {
            check_references(&registry, Some(self))?;
        }
        Ok(())
    }
}

fn check_references(registry: &Registry, secrets: Option<&SecretDocument>) -> Result<(), Error> {
    for server in &registry.servers {
        let references = match &server.transport {
            Destination::Stdio { secret_env, .. } => secret_env,
            Destination::StreamableHttp { secret_headers, .. } => secret_headers,
        };
        if references
            .values()
            .any(|reference| !secrets.is_some_and(|s| s.values.contains_key(reference)))
        {
            return Err(Error::Auth);
        }
    }
    Ok(())
}

// Retain the real handles while construction is unavailable on non-Unix.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Store {
    home: File,
    directory: File,
    path: PathBuf,
}

/// A host-derived destination for one immutable server runtime version.
///
/// Fields stay private so callers cannot substitute another store, server, or
/// version path between approval and creation.
#[derive(Clone)]
pub struct RuntimeVersionPlan {
    server_id: String,
    operation_id: String,
    destination: PathBuf,
    store_dev: u64,
    store_ino: u64,
}

impl RuntimeVersionPlan {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

/// A newly created private directory below the plan's exact server parent.
/// Dropping it intentionally leaves recovery evidence in place.
pub struct RuntimeVersionStaging {
    store_home: File,
    store_directory: File,
    directory: File,
    parent: File,
    partial_name: OsString,
    plan: RuntimeVersionPlan,
}

impl RuntimeVersionStaging {
    pub fn try_clone_directory(&self) -> Result<File, Error> {
        self.directory.try_clone().map_err(|_| Error::Storage)
    }
}

/// A published runtime pinned by an open descriptor.
pub struct PublishedRuntime {
    directory: File,
    path: PathBuf,
}

impl PublishedRuntime {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn try_clone_directory(&self) -> Result<File, Error> {
        self.directory.try_clone().map_err(|_| Error::Storage)
    }
}

#[cfg(unix)]
struct WriterLock(File);
#[cfg(unix)]
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(any(unix, test))]
fn write_protected(
    file: &mut File,
    bytes: &[u8],
    register: impl FnOnce(&File) -> Result<(), Error>,
) -> Result<(), Error> {
    register(file)?;
    file.write_all(bytes).map_err(|_| Error::Storage)?;
    file.sync_all().map_err(|_| Error::Storage)
}

#[cfg(unix)]
fn metadata(file: &File, directory: bool) -> Result<(), Error> {
    let m = file.metadata().map_err(|_| Error::Storage)?;
    // SAFETY: geteuid has no arguments or memory effects.
    let owner = unsafe { libc::geteuid() };
    if m.uid() != owner
        || (directory && (!m.is_dir() || m.mode() & 0o077 != 0))
        || (!directory && (!m.is_file() || m.nlink() != 1 || m.mode() & 0o7777 != 0o600))
    {
        return Err(Error::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn open_at(parent: &File, name: &str, flags: i32) -> Result<File, std::io::Error> {
    open_at_os(parent, OsStr::new(name), flags)
}

#[cfg(unix)]
fn open_at_os(parent: &File, name: &OsStr, flags: i32) -> Result<File, std::io::Error> {
    let name = CString::new(name.as_bytes()).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: parent owns a live descriptor and name is terminated. A successful
    // openat returns a new descriptor, immediately transferred to File.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn private_directory(parent: &File, name: &str, private: bool) -> Result<File, Error> {
    let c_name = CString::new(name).map_err(|_| Error::Storage)?;
    // SAFETY: bounded, terminated component and a live parent descriptor.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) };
    if result != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
        return Err(Error::Storage);
    }
    let dir =
        open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(|_| Error::Protection)?;
    if private {
        metadata(&dir, true)?;
    } else {
        parent_metadata(&dir)?;
    }
    parent.sync_all().map_err(|_| Error::Storage)?;
    Ok(dir)
}

#[cfg(unix)]
fn parent_metadata(file: &File) -> Result<(), Error> {
    let m = file.metadata().map_err(|_| Error::Storage)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(Error::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn check_store_directory(home: &File, directory: &File) -> Result<(), Error> {
    let parent = open_at(home, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(|_| Error::Protection)?;
    parent_metadata(&parent)?;
    let current = open_at(&parent, "mcp", libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(|_| Error::Protection)?;
    metadata(&current, true)?;
    let a = current.metadata().map_err(|_| Error::Storage)?;
    let b = directory.metadata().map_err(|_| Error::Storage)?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err(Error::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn open_home(home: &Path) -> Result<(File, PathBuf), Error> {
    if !home.is_absolute() || home.canonicalize().map_err(|_| Error::Protection)? != home {
        return Err(Error::Protection);
    }
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home)
        .map_err(|_| Error::Protection)?;
    let m = root.metadata().map_err(|_| Error::Storage)?;
    if m.uid() != unsafe { libc::geteuid() } || !m.is_dir() {
        return Err(Error::Protection);
    }
    Ok((root, home.join(".polaris/mcp")))
}

#[cfg(unix)]
fn reserve_store_paths(path: &Path) -> Result<(), Error> {
    // Reserve every protected path before opening a document or lock inode.
    drop(protect_store(&path.join("servers.json")).map_err(|_| Error::Protection)?);
    drop(protect_store(&path.join("secrets.json")).map_err(|_| Error::Protection)?);
    drop(protect_store(&path.join("owner.lock")).map_err(|_| Error::Protection)?);
    drop(protect_store(&path.join(CATALOG)).map_err(|_| Error::Protection)?);
    Ok(())
}

#[cfg(unix)]
fn valid_runtime_operation_id(operation_id: &str) -> bool {
    operation_id.len() == 32
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
fn runtime_version_name(operation_id: &str) -> String {
    format!("runtime-{operation_id}")
}

#[cfg(unix)]
fn runtime_entry_names(directory: &File) -> Result<Vec<OsString>, Error> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(Error::Storage);
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(Error::Storage);
    }
    let mut names = Vec::new();
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            let close = unsafe { libc::closedir(stream) };
            if close != 0 || error.raw_os_error().unwrap_or(0) != 0 {
                return Err(Error::Storage);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn sync_runtime_tree(directory: &File) -> Result<(), Error> {
    let mut budget = RuntimeSyncBudget::default();
    sync_runtime_tree_inner(directory, 0, &mut budget)
}

#[cfg(unix)]
#[derive(Default)]
struct RuntimeSyncBudget {
    entries: usize,
    files: usize,
    bytes: u64,
}

#[cfg(unix)]
fn sync_runtime_tree_inner(
    directory: &File,
    depth: usize,
    budget: &mut RuntimeSyncBudget,
) -> Result<(), Error> {
    if depth > RUNTIME_MAX_DEPTH {
        return Err(Error::Limit);
    }
    let metadata = directory.metadata().map_err(|_| Error::Storage)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(Error::Protection);
    }
    for name in runtime_entry_names(directory)? {
        budget.entries = budget.entries.checked_add(1).ok_or(Error::Limit)?;
        if budget.entries > RUNTIME_MAX_ENTRIES {
            return Err(Error::Limit);
        }
        let bytes = name.as_os_str().as_bytes();
        let c_name = CString::new(bytes).map_err(|_| Error::Protection)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(Error::Protection);
        }
        let stat = unsafe { stat.assume_init() };
        let kind = stat.st_mode & libc::S_IFMT;
        if stat.st_uid != unsafe { libc::geteuid() } || stat.st_mode & 0o022 != 0 {
            return Err(Error::Protection);
        }
        if kind == libc::S_IFDIR {
            let child = open_at_os(directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                .map_err(|_| Error::Protection)?;
            if !same_runtime_identity(&child.metadata().map_err(|_| Error::Storage)?, &stat) {
                return Err(Error::Protection);
            }
            sync_runtime_tree_inner(&child, depth + 1, budget)?;
        } else if kind == libc::S_IFREG && stat.st_nlink == 1 {
            let file =
                open_at_os(directory, &name, libc::O_RDONLY).map_err(|_| Error::Protection)?;
            let reopened = file.metadata().map_err(|_| Error::Storage)?;
            if !same_runtime_identity(&reopened, &stat)
                || !reopened.is_file()
                || reopened.uid() != unsafe { libc::geteuid() }
                || reopened.mode() & 0o022 != 0
                || reopened.nlink() != 1
            {
                return Err(Error::Protection);
            }
            budget.files = budget.files.checked_add(1).ok_or(Error::Limit)?;
            budget.bytes = budget
                .bytes
                .checked_add(reopened.len())
                .ok_or(Error::Limit)?;
            if budget.files > RUNTIME_MAX_FILES
                || reopened.len() > RUNTIME_MAX_FILE_BYTES
                || budget.bytes > RUNTIME_MAX_TOTAL_BYTES
            {
                return Err(Error::Limit);
            }
            file.sync_all().map_err(|_| Error::Storage)?;
        } else {
            return Err(Error::Protection);
        }
    }
    directory.sync_all().map_err(|_| Error::Storage)
}

#[cfg(target_os = "macos")]
fn same_runtime_identity(metadata: &std::fs::Metadata, stat: &libc::stat) -> bool {
    metadata.dev() == stat.st_dev as u64 && metadata.ino() == stat.st_ino
}

#[cfg(target_os = "linux")]
fn same_runtime_identity(metadata: &std::fs::Metadata, stat: &libc::stat) -> bool {
    metadata.dev() == stat.st_dev && metadata.ino() == stat.st_ino
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn same_runtime_identity(metadata: &std::fs::Metadata, stat: &libc::stat) -> bool {
    metadata.dev() == stat.st_dev as u64 && metadata.ino() == stat.st_ino as u64
}

#[cfg(unix)]
fn rename_no_replace(
    from: &File,
    source: &OsStr,
    to: &File,
    destination: &OsStr,
) -> Result<(), Error> {
    let source = CString::new(source.as_bytes()).map_err(|_| Error::Protection)?;
    let destination = CString::new(destination.as_bytes()).map_err(|_| Error::Protection)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                from.as_raw_fd(),
                source.as_ptr(),
                to.as_raw_fd(),
                destination.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                from.as_raw_fd(),
                source.as_ptr(),
                to.as_raw_fd(),
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::RevisionConflict
            } else {
                Error::Storage
            });
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (from, source, to, destination);
        Err(Error::Protection)
    }
}

#[cfg(unix)]
impl RuntimeVersionStaging {
    /// Verify, durably publish with no replacement, then verify the exact
    /// published descriptor again. The callback is the caller's manifest and
    /// admission boundary and receives only the bound directory.
    pub fn publish(
        self,
        verify: impl Fn(&File) -> Result<(), Error>,
    ) -> Result<PublishedRuntime, Error> {
        verify(&self.directory)?;
        sync_runtime_tree(&self.directory)?;
        let destination_name = runtime_version_name(&self.plan.operation_id);
        rename_no_replace(
            &self.parent,
            &self.partial_name,
            &self.parent,
            OsStr::new(&destination_name),
        )?;
        self.parent.sync_all().map_err(|_| Error::Storage)?;
        let directory = open_at(
            &self.parent,
            &destination_name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| Error::Protection)?;
        metadata(&directory, true)?;
        let before = self.directory.metadata().map_err(|_| Error::Storage)?;
        let after = directory.metadata().map_err(|_| Error::Storage)?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(Error::Protection);
        }
        check_store_directory(&self.store_home, &self.store_directory)?;
        let servers = open_at(
            &self.store_directory,
            "servers",
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| Error::Protection)?;
        let namespace_parent = open_at(
            &servers,
            &self.plan.server_id,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| Error::Protection)?;
        metadata(&namespace_parent, true)?;
        let held_parent = self.parent.metadata().map_err(|_| Error::Storage)?;
        let current_parent = namespace_parent.metadata().map_err(|_| Error::Storage)?;
        if (held_parent.dev(), held_parent.ino()) != (current_parent.dev(), current_parent.ino()) {
            return Err(Error::Protection);
        }
        let namespace_directory = open_at(
            &namespace_parent,
            &destination_name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| Error::Protection)?;
        metadata(&namespace_directory, true)?;
        let current = namespace_directory.metadata().map_err(|_| Error::Storage)?;
        if (after.dev(), after.ino()) != (current.dev(), current.ino()) {
            return Err(Error::Protection);
        }
        sync_runtime_tree(&directory)?;
        verify(&directory)?;
        self.parent.sync_all().map_err(|_| Error::Storage)?;
        Ok(PublishedRuntime {
            directory,
            path: self.plan.destination,
        })
    }
}

impl Store {
    /// Whether a store directory was ever created, without creating it.
    pub fn exists(home: &Path) -> bool {
        home.join(".polaris/mcp")
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_dir())
    }
}

#[cfg(unix)]
impl Store {
    /// `home` must be the trusted OS-resolved canonical home, never project input.
    /// Reserve store paths before calling any model-visible reader.
    pub fn open(home: &Path) -> Result<Self, Error> {
        let (root, path) = open_home(home)?;
        reserve_store_paths(&path)?;
        let polaris = private_directory(&root, ".polaris", false)?;
        let directory = private_directory(&polaris, "mcp", true)?;
        Ok(Self {
            home: root,
            directory,
            path,
        })
    }

    /// Open a previously initialized store without creating any directory,
    /// destination, or lock file. Used by approval preflight only.
    pub fn open_existing(home: &Path) -> Result<Self, Error> {
        let (root, path) = open_home(home)?;
        reserve_store_paths(&path)?;
        let polaris = open_at(&root, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| Error::Protection)?;
        parent_metadata(&polaris)?;
        let directory = open_at(&polaris, "mcp", libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| Error::Protection)?;
        metadata(&directory, true)?;
        Ok(Self {
            home: root,
            directory,
            path,
        })
    }

    fn writer_lock_with_creation(&self, create: bool) -> Result<WriterLock, Error> {
        // Do not keep the in-process protection lock while awaiting a file lock.
        let flags = libc::O_RDWR | if create { libc::O_CREAT } else { 0 };
        let file = open_at(&self.directory, "owner.lock", flags).map_err(|_| Error::Protection)?;
        metadata(&file, false)?;
        // Short reads hold the lock for milliseconds; a busy lock past this
        // bound is reported as a conflict rather than awaited.
        let mut attempts = 0;
        while file.try_lock().is_err() {
            attempts += 1;
            if attempts >= 20 {
                return Err(Error::RevisionConflict);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let guard = WriterLock(file);
        self.check_directory()?;
        Ok(guard)
    }

    fn writer_lock(&self) -> Result<WriterLock, Error> {
        self.writer_lock_with_creation(true)
    }

    fn check_directory(&self) -> Result<(), Error> {
        check_store_directory(&self.home, &self.directory)
    }

    pub fn load_registry(&self) -> Result<Option<Registry>, Error> {
        self.load("servers.json")
    }
    pub fn load_secrets(&self) -> Result<Option<Secrets>, Error> {
        self.load("secrets.json").map(|value| value.map(Secrets))
    }

    /// Trusted startup read, before any model-visible preparation. Retains the
    /// same store lock and protection guard through both reads and validation.
    pub fn load_snapshot(&self) -> Result<Snapshot, Error> {
        self.load_snapshot_with_creation(true)
    }

    /// Read and validate one snapshot while opening the existing owner lock
    /// without O_CREAT. The lock is released before this method returns.
    pub fn load_snapshot_noncreating(&self) -> Result<Snapshot, Error> {
        self.load_snapshot_with_creation(false)
    }

    fn load_snapshot_with_creation(&self, create_lock: bool) -> Result<Snapshot, Error> {
        let _writer = self.writer_lock_with_creation(create_lock)?;
        let mut protection =
            protect_store(&self.path.join("servers.json")).map_err(|_| Error::Protection)?;
        self.require_no_temp("servers.json")?;
        self.require_no_temp("secrets.json")?;
        let registry: Option<Registry> = self.read("servers.json", &mut protection)?;
        let secrets: Option<SecretDocument> = self.read("secrets.json", &mut protection)?;
        if let Some(registry) = &registry {
            check_references(registry, secrets.as_ref())?;
        }
        self.check_directory()?;
        Ok(Snapshot {
            registry,
            secrets: secrets.map(Secrets),
        })
    }
    pub fn save_registry(&self, expected_revision: u64, next: &Registry) -> Result<(), Error> {
        self.save("servers.json", expected_revision, next)
    }
    pub fn save_secrets(&self, expected_revision: u64, next: &Secrets) -> Result<(), Error> {
        self.save("secrets.json", expected_revision, &next.0)
    }

    /// Replace or remove one value. `save` re-checks the revision and references.
    pub fn update_secret(
        &self,
        expected_revision: u64,
        reference: &str,
        value: Option<SecretValue>,
    ) -> Result<(), Error> {
        self.update_secrets(expected_revision, vec![(reference.into(), value)])
    }

    /// Several changes as one revision; removing an absent value fails.
    pub fn update_secrets(
        &self,
        expected_revision: u64,
        changes: Vec<(String, Option<SecretValue>)>,
    ) -> Result<(), Error> {
        let mut values = match self.load_secrets()? {
            Some(mut current) if current.revision() == expected_revision => {
                std::mem::take(&mut current.0.values)
            }
            None if expected_revision == 0 => BTreeMap::new(),
            _ => return Err(Error::RevisionConflict),
        };
        for (reference, value) in changes {
            match value {
                Some(value) => {
                    // Move the trusted wipe guard into the document. An old
                    // value is wiped when the overwritten wrapper is dropped.
                    values.insert(reference, StoredSecretValue(value.0));
                }
                None => {
                    drop(values.remove(&reference).ok_or(Error::Auth)?);
                }
            }
        }
        let next = Secrets::from_stored(
            expected_revision.checked_add(1).ok_or(Error::Limit)?,
            values,
        )?;
        self.save_secrets(expected_revision, &next)
    }

    /// A private working directory for one server, created without following
    /// links. Registration stores the returned path as the server's cwd.
    pub fn server_directory(&self, server_id: &str) -> Result<PathBuf, Error> {
        if !valid_id(server_id) {
            return Err(Error::InvalidRegistration);
        }
        let servers = private_directory(&self.directory, "servers", true)?;
        drop(private_directory(&servers, server_id, true)?);
        self.check_directory()?;
        Ok(self.path.join("servers").join(server_id))
    }

    /// Derive one immutable runtime destination from the held store and exact
    /// server/operation identities without creating it.
    pub fn plan_runtime_version(
        &self,
        server_id: &str,
        operation_id: &str,
    ) -> Result<RuntimeVersionPlan, Error> {
        if !valid_id(server_id) || !valid_runtime_operation_id(operation_id) {
            return Err(Error::InvalidRegistration);
        }
        self.check_directory()?;
        let store = self.directory.metadata().map_err(|_| Error::Storage)?;
        Ok(RuntimeVersionPlan {
            server_id: server_id.to_owned(),
            operation_id: operation_id.to_owned(),
            destination: self
                .path
                .join("servers")
                .join(server_id)
                .join(runtime_version_name(operation_id)),
            store_dev: store.dev(),
            store_ino: store.ino(),
        })
    }

    /// Create a unique private partial below the plan's exact server parent.
    /// This is intentionally a post-approval operation.
    pub fn create_runtime_version_staging(
        &self,
        plan: &RuntimeVersionPlan,
    ) -> Result<RuntimeVersionStaging, Error> {
        self.validate_runtime_plan(plan)?;
        let servers = private_directory(&self.directory, "servers", true)?;
        let parent = private_directory(&servers, &plan.server_id, true)?;
        let mut created = None;
        for _ in 0..64 {
            let sequence = RUNTIME_PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                ".partial-{}-{:x}-{sequence:016x}",
                plan.operation_id,
                std::process::id()
            );
            let c_name = CString::new(name.as_str()).map_err(|_| Error::Storage)?;
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) };
            if result == 0 {
                let directory = open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                    .map_err(|_| Error::Protection)?;
                metadata(&directory, true)?;
                parent.sync_all().map_err(|_| Error::Storage)?;
                created = Some((OsString::from(name), directory));
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
                return Err(Error::Storage);
            }
        }
        let (partial_name, directory) = created.ok_or(Error::RevisionConflict)?;
        Ok(RuntimeVersionStaging {
            store_home: self.home.try_clone().map_err(|_| Error::Storage)?,
            store_directory: self.directory.try_clone().map_err(|_| Error::Storage)?,
            directory,
            parent,
            partial_name,
            plan: plan.clone(),
        })
    }

    fn validate_runtime_plan(&self, plan: &RuntimeVersionPlan) -> Result<(), Error> {
        self.check_directory()?;
        let store = self.directory.metadata().map_err(|_| Error::Storage)?;
        let exact = self
            .path
            .join("servers")
            .join(&plan.server_id)
            .join(runtime_version_name(&plan.operation_id));
        if !valid_id(&plan.server_id)
            || !valid_runtime_operation_id(&plan.operation_id)
            || (store.dev(), store.ino()) != (plan.store_dev, plan.store_ino)
            || plan.destination != exact
        {
            return Err(Error::Protection);
        }
        Ok(())
    }

    /// Derived cache: callers treat any error as an unavailable index.
    pub fn load_catalog_index(&self) -> Result<Option<CatalogIndex>, Error> {
        let _writer = self.writer_lock()?;
        let mut protection =
            protect_store(&self.path.join(CATALOG)).map_err(|_| Error::Protection)?;
        self.require_no_temp(CATALOG)?;
        let Some(bytes) = self.read_bytes(CATALOG, &mut protection, MAX_INDEX_BYTES)? else {
            return Ok(None);
        };
        let index: CatalogIndex = serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?;
        index.validate()?;
        Ok(Some(index))
    }

    /// Replace one server's entry, dropping entries that no longer match the
    /// registry read under the same lock. An interrupted or corrupt earlier
    /// cache is discarded, never trusted. Tools beyond the file cap are cut and
    /// the entry marked partial; returns the saved tool count and partial flag.
    pub fn save_catalog_entry(
        &self,
        server_id: &str,
        mut entry: ServerIndex,
    ) -> Result<(usize, bool), Error> {
        let _writer = self.writer_lock()?;
        let mut protection =
            protect_store(&self.path.join(CATALOG)).map_err(|_| Error::Protection)?;
        self.remove_temp(CATALOG)?;
        self.require_no_temp("servers.json")?;
        let registry: Option<Registry> = self.read("servers.json", &mut protection)?;
        let mut index = match self.read_bytes(CATALOG, &mut protection, MAX_INDEX_BYTES) {
            Ok(Some(bytes)) => serde_json::from_slice::<CatalogIndex>(&bytes)
                .ok()
                .filter(|index| index.validate().is_ok())
                .unwrap_or_default(),
            Ok(None) | Err(Error::Limit) => CatalogIndex::default(),
            Err(error) => return Err(error),
        };
        index.schema_version = 1;
        index.servers.retain(|id, existing| {
            registry.as_ref().is_some_and(|registry| {
                registry.servers.iter().any(|server| {
                    &server.server_id == id
                        && server
                            .destination_hash()
                            .is_ok_and(|hash| hash == existing.destination_hash)
                })
            })
        });
        let tools = std::mem::take(&mut entry.tools);
        index.servers.insert(server_id.into(), entry);
        let mut used = serde_json::to_vec(&index)
            .map_err(|_| Error::Storage)?
            .len();
        let entry = index.servers.get_mut(server_id).expect("inserted entry");
        for tool in tools {
            let bytes = serde_json::to_vec(&tool).map_err(|_| Error::Storage)?.len() + 1;
            if used + bytes > MAX_INDEX_BYTES {
                entry.partial = true;
                break;
            }
            used += bytes;
            entry.tools.push(tool);
        }
        let saved = (entry.tools.len(), entry.partial);
        index.validate()?;
        let bytes = bounded_json(&index, MAX_INDEX_BYTES)?;
        self.replace(CATALOG, &bytes, &mut protection)?;
        Ok(saved)
    }

    fn remove_temp(&self, name: &str) -> Result<(), Error> {
        let name = CString::new(format!("{name}.tmp")).map_err(|_| Error::Storage)?;
        // SAFETY: live directory descriptor and terminated name; unlinkat never follows a link.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
        {
            return Err(Error::Protection);
        }
        Ok(())
    }

    fn load<T: Document>(&self, name: &str) -> Result<Option<T>, Error> {
        let _writer = self.writer_lock()?;
        let mut protection = protect_store(&self.path.join(name)).map_err(|_| Error::Protection)?;
        self.require_no_temp(name)?;
        let value: Option<T> = self.read(name, &mut protection)?;
        if let Some(value) = &value {
            value.validate_links(self, &mut protection)?;
        }
        Ok(value)
    }

    fn require_no_temp(&self, name: &str) -> Result<(), Error> {
        // lstat through the directory detects dangling symlinks as well as files.
        let name = CString::new(format!("{name}.tmp")).map_err(|_| Error::Storage)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            return Err(Error::RecoveryRequired);
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(Error::Protection)
        }
    }

    fn read<T: Document>(
        &self,
        name: &str,
        protection: &mut polaris_auth::protection::StoreProtectionGuard,
    ) -> Result<Option<T>, Error> {
        let Some(bytes) = self.read_bytes(name, protection, MAX_STORE_BYTES)? else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        let value: T = serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?;
        value.validate()?;
        Ok(Some(value))
    }

    fn read_bytes(
        &self,
        name: &str,
        protection: &mut polaris_auth::protection::StoreProtectionGuard,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, Error> {
        let file = match open_at(&self.directory, name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Protection),
        };
        metadata(&file, false)?;
        protection
            .remember_file(&file)
            .map_err(|_| Error::Protection)?;
        if file.metadata().map_err(|_| Error::Storage)?.len() > limit as u64 {
            return Err(Error::Limit);
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() > limit {
            return Err(Error::Limit);
        }
        Ok(Some(bytes))
    }

    fn save<T: Document>(&self, name: &str, expected: u64, next: &T) -> Result<(), Error> {
        next.validate()?;
        if expected.checked_add(1) != Some(next.revision()) {
            return Err(Error::RevisionConflict);
        }
        let bytes = Zeroizing::new(bounded_json(next, MAX_STORE_BYTES)?);
        let _writer = self.writer_lock()?;
        let mut protection = protect_store(&self.path.join(name)).map_err(|_| Error::Protection)?;
        self.require_no_temp(name)?;
        let previous: Option<T> = self.read(name, &mut protection)?;
        if previous.as_ref().map(Document::revision).unwrap_or(0) != expected {
            return Err(Error::RevisionConflict);
        }
        next.validate_links(self, &mut protection)?;
        self.replace(name, &bytes, &mut protection)
    }

    fn replace(
        &self,
        name: &str,
        bytes: &[u8],
        protection: &mut polaris_auth::protection::StoreProtectionGuard,
    ) -> Result<(), Error> {
        let temp = format!("{name}.tmp");
        let mut file = open_at(
            &self.directory,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(|_| Error::Protection)?;
        metadata(&file, false)?;
        // This must precede the first credential byte, including for new inodes.
        write_protected(&mut file, bytes, |file| {
            protection
                .remember_file(file)
                .map_err(|_| Error::Protection)
        })?;
        self.check_directory()?;
        let temp = CString::new(temp).map_err(|_| Error::Storage)?;
        let name = CString::new(name).map_err(|_| Error::Storage)?;
        let result = unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                temp.as_ptr(),
                self.directory.as_raw_fd(),
                name.as_ptr(),
            )
        };
        if result != 0 {
            return Err(Error::Storage);
        }
        self.directory.sync_all().map_err(|_| Error::Storage)?;
        Ok(())
    }
}

#[cfg(not(unix))]
impl Store {
    pub fn open(_home: &Path) -> Result<Self, Error> {
        Err(Error::Protection)
    }

    pub fn open_existing(_home: &Path) -> Result<Self, Error> {
        Err(Error::Protection)
    }

    pub fn load_registry(&self) -> Result<Option<Registry>, Error> {
        Err(Error::Protection)
    }

    pub fn load_secrets(&self) -> Result<Option<Secrets>, Error> {
        Err(Error::Protection)
    }

    pub fn load_snapshot(&self) -> Result<Snapshot, Error> {
        Err(Error::Protection)
    }

    pub fn load_snapshot_noncreating(&self) -> Result<Snapshot, Error> {
        Err(Error::Protection)
    }

    pub fn save_registry(&self, _expected_revision: u64, _next: &Registry) -> Result<(), Error> {
        Err(Error::Protection)
    }

    pub fn save_secrets(&self, _expected_revision: u64, _next: &Secrets) -> Result<(), Error> {
        Err(Error::Protection)
    }

    pub fn update_secret(
        &self,
        _expected_revision: u64,
        _reference: &str,
        _value: Option<SecretValue>,
    ) -> Result<(), Error> {
        Err(Error::Protection)
    }

    pub fn update_secrets(
        &self,
        _expected_revision: u64,
        _changes: Vec<(String, Option<SecretValue>)>,
    ) -> Result<(), Error> {
        Err(Error::Protection)
    }

    pub fn server_directory(&self, _server_id: &str) -> Result<PathBuf, Error> {
        Err(Error::Protection)
    }

    pub fn plan_runtime_version(
        &self,
        _server_id: &str,
        _operation_id: &str,
    ) -> Result<RuntimeVersionPlan, Error> {
        Err(Error::Protection)
    }

    pub fn create_runtime_version_staging(
        &self,
        _plan: &RuntimeVersionPlan,
    ) -> Result<RuntimeVersionStaging, Error> {
        Err(Error::Protection)
    }

    pub fn load_catalog_index(&self) -> Result<Option<CatalogIndex>, Error> {
        Err(Error::Protection)
    }

    pub fn save_catalog_entry(
        &self,
        _server_id: &str,
        _entry: ServerIndex,
    ) -> Result<(usize, bool), Error> {
        Err(Error::Protection)
    }
}

#[cfg(not(unix))]
impl RuntimeVersionStaging {
    pub fn publish(
        self,
        _verify: impl Fn(&File) -> Result<(), Error>,
    ) -> Result<PublishedRuntime, Error> {
        Err(Error::Protection)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::registration::{Capabilities, Destination, ProtocolProfile, Server, Timeouts};
    use std::collections::BTreeSet;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn empty(revision: u64) -> Registry {
        Registry {
            schema_version: 1,
            revision,
            servers: vec![],
        }
    }
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().canonicalize().unwrap()).unwrap();
        (dir, store)
    }

    fn staging_path(staging: &RuntimeVersionStaging) -> PathBuf {
        staging
            .plan
            .destination
            .parent()
            .unwrap()
            .join(&staging.partial_name)
    }

    #[test]
    fn existing_store_snapshot_never_creates_store_or_lock() {
        let absent = tempfile::tempdir().unwrap();
        let home = absent.path().canonicalize().unwrap();
        assert_eq!(Store::open_existing(&home).err(), Some(Error::Protection));
        assert!(!home.join(".polaris").exists());

        let (dir, store) = fixture();
        let home = dir.path().canonicalize().unwrap();
        assert!(!store.path.join("owner.lock").exists());
        let existing = Store::open_existing(&home).unwrap();
        assert_eq!(
            existing.load_snapshot_noncreating().err(),
            Some(Error::Protection)
        );
        assert!(!store.path.join("owner.lock").exists());

        store.save_registry(0, &empty(1)).unwrap();
        let existing = Store::open_existing(&home).unwrap();
        assert_eq!(
            existing.load_snapshot_noncreating().unwrap().registry,
            Some(empty(1))
        );
        assert_eq!(
            std::fs::metadata(store.path.join("owner.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn runtime_version_is_bound_to_store_server_and_lower_hex_operation() {
        let (dir, store) = fixture();
        let operation = "0123456789abcdef0123456789abcdef";
        let plan = store.plan_runtime_version("unity", operation).unwrap();
        assert_eq!(plan.server_id(), "unity");
        assert_eq!(plan.operation_id(), operation);
        assert_eq!(
            plan.destination(),
            dir.path()
                .canonicalize()
                .unwrap()
                .join(".polaris/mcp/servers/unity")
                .join(format!("runtime-{operation}"))
        );
        for invalid in [
            "0123456789abcdef0123456789abcde",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef0",
        ] {
            assert_eq!(
                store.plan_runtime_version("unity", invalid).err(),
                Some(Error::InvalidRegistration),
                "{invalid}"
            );
        }
        assert_eq!(
            store.plan_runtime_version("../other", operation).err(),
            Some(Error::InvalidRegistration)
        );

        let (other_home, other) = fixture();
        assert_eq!(
            other.create_runtime_version_staging(&plan).err(),
            Some(Error::Protection)
        );
        assert!(!other_home.path().join(".polaris/mcp/servers").exists());
    }

    #[test]
    fn runtime_version_publishes_without_replacement_and_revalidates() {
        let (_dir, store) = fixture();
        let operation = "0123456789abcdef0123456789abcdef";
        let plan = store.plan_runtime_version("unity", operation).unwrap();
        let staging = store.create_runtime_version_staging(&plan).unwrap();
        let staging_dir = staging_path(&staging);
        assert!(
            staging_dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".partial-")
        );
        std::fs::create_dir(staging_dir.join("build")).unwrap();
        std::fs::write(staging_dir.join("build/index.js"), b"fixture entry").unwrap();
        std::fs::set_permissions(
            staging_dir.join("build/index.js"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let calls = std::cell::Cell::new(0);
        let published = staging
            .publish(|directory| {
                calls.set(calls.get() + 1);
                assert!(directory.metadata().unwrap().is_dir());
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 2);
        assert!(published.path().join("build/index.js").is_file());
        assert_eq!(
            std::fs::metadata(published.path().join("build/index.js"))
                .unwrap()
                .nlink(),
            1
        );
        published.try_clone_directory().unwrap();
        let repeated_plan = store.plan_runtime_version("unity", operation).unwrap();
        let repeated = store
            .create_runtime_version_staging(&repeated_plan)
            .unwrap();
        std::fs::write(staging_path(&repeated).join("different"), b"not published").unwrap();
        assert_eq!(
            repeated.publish(|_| Ok(())).err(),
            Some(Error::RevisionConflict)
        );
        assert!(!published.path().join("different").exists());
    }

    #[test]
    fn runtime_version_rejects_hardlinks_and_special_entries_before_publish() {
        let (_dir, store) = fixture();
        for (index, special) in [false, true].into_iter().enumerate() {
            let operation = format!("{index:032x}");
            let plan = store.plan_runtime_version("unity", &operation).unwrap();
            let staging = store.create_runtime_version_staging(&plan).unwrap();
            let staging_path = staging_path(&staging);
            let target = staging_path.join("file");
            std::fs::write(&target, b"fixture").unwrap();
            if special {
                symlink("file", staging_path.join("alias")).unwrap();
            } else {
                std::fs::hard_link(&target, staging_path.join("alias")).unwrap();
            }
            let destination = store
                .plan_runtime_version("unity", &operation)
                .unwrap()
                .destination()
                .to_path_buf();
            assert_eq!(staging.publish(|_| Ok(())).err(), Some(Error::Protection));
            assert!(!destination.exists());
        }
    }

    #[test]
    fn runtime_version_requires_manifest_admission_before_and_after_publish() {
        let (_dir, store) = fixture();
        let pre_operation = "00000000000000000000000000000001";
        let pre_plan = store.plan_runtime_version("unity", pre_operation).unwrap();
        let pre_destination = pre_plan.destination().to_path_buf();
        let pre_staging = store.create_runtime_version_staging(&pre_plan).unwrap();
        let pre_partial = staging_path(&pre_staging);
        std::fs::write(pre_partial.join("entry"), b"fixture").unwrap();
        assert_eq!(
            pre_staging.publish(|_| Err(Error::Protocol)).err(),
            Some(Error::Protocol)
        );
        assert!(!pre_destination.exists());
        assert!(pre_partial.join("entry").is_file());

        let post_operation = "00000000000000000000000000000002";
        let post_plan = store.plan_runtime_version("unity", post_operation).unwrap();
        let post_destination = post_plan.destination().to_path_buf();
        let post_staging = store.create_runtime_version_staging(&post_plan).unwrap();
        std::fs::write(staging_path(&post_staging).join("entry"), b"fixture").unwrap();
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            post_staging
                .publish(|_| {
                    calls.set(calls.get() + 1);
                    if calls.get() == 1 {
                        Ok(())
                    } else {
                        Err(Error::Protocol)
                    }
                })
                .err(),
            Some(Error::Protocol)
        );
        assert_eq!(calls.get(), 2);
        assert!(post_destination.join("entry").is_file());
    }

    #[test]
    fn runtime_version_rejects_detached_server_namespace_after_publish() {
        let (_dir, store) = fixture();
        let operation = "0123456789abcdef0123456789abcdef";
        let plan = store.plan_runtime_version("unity", operation).unwrap();
        let destination = plan.destination().to_path_buf();
        let staging = store.create_runtime_version_staging(&plan).unwrap();
        std::fs::write(staging_path(&staging).join("entry"), b"fixture").unwrap();

        let servers = store.path.join("servers");
        let retired = servers.join("retired-unity");
        std::fs::rename(servers.join("unity"), &retired).unwrap();
        std::fs::create_dir(servers.join("unity")).unwrap();
        std::fs::set_permissions(
            servers.join("unity"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();

        assert_eq!(staging.publish(|_| Ok(())).err(), Some(Error::Protection));
        assert!(!destination.exists());
        assert!(
            retired
                .join(runtime_version_name(operation))
                .join("entry")
                .is_file()
        );
    }

    #[test]
    fn snapshot_preserves_absence_and_keeps_a_loaded_pair_after_rotation() {
        let (_dir, store) = fixture();
        let absent = store.load_snapshot().unwrap();
        assert!(absent.registry.is_none() && absent.secrets.is_none());
        assert!(!store.path.join("servers.json").exists());
        assert!(!store.path.join("secrets.json").exists());
        store
            .save_secrets(
                0,
                &Secrets::new(
                    1,
                    BTreeMap::from([("credential".into(), "synthetic-old".into())]),
                )
                .unwrap(),
            )
            .unwrap();
        let secret_only = store.load_snapshot().unwrap();
        assert!(secret_only.registry.is_none());
        assert_eq!(secret_only.secrets.unwrap().revision(), 1);
        let mut registry = empty(1);
        registry.servers.push(Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::new(),
            transport: Destination::StreamableHttp {
                url: "https://example.invalid/mcp".into(),
                secret_headers: BTreeMap::from([("authorization".into(), "credential".into())]),
            },
            protocol_profile: Default::default(),
            capabilities: Default::default(),
            timeouts: Default::default(),
        });
        store.save_registry(0, &registry).unwrap();
        let saved = store.load_snapshot().unwrap();
        assert_eq!(saved.registry, Some(registry.clone()));
        store
            .save_secrets(
                1,
                &Secrets::new(
                    2,
                    BTreeMap::from([("credential".into(), "synthetic-new".into())]),
                )
                .unwrap(),
            )
            .unwrap();
        let current = store.load_snapshot().unwrap();
        assert_eq!(current.registry, Some(registry));
        let old = saved.secrets.unwrap();
        let new = current.secrets.unwrap();
        assert_eq!(old.revision(), 1);
        assert_eq!(old.resolve("credential").unwrap(), "synthetic-old");
        assert_eq!(new.revision(), 2);
        assert_eq!(new.resolve("credential").unwrap(), "synthetic-new");
    }

    #[test]
    fn snapshot_checks_references_in_the_returned_pair_and_not_only_each_document() {
        let (_dir, store) = fixture();
        store.save_registry(0, &empty(1)).unwrap();
        let without_secrets = store.load_snapshot().unwrap();
        assert_eq!(without_secrets.registry, Some(empty(1)));
        assert!(without_secrets.secrets.is_none());
        store
            .save_secrets(0, &Secrets::new(1, BTreeMap::new()).unwrap())
            .unwrap();
        // Each document is valid alone; their reference relationship is invalid.
        let mut registry = empty(1);
        registry.servers.push(Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::new(),
            transport: Destination::StreamableHttp {
                url: "https://example.invalid/mcp".into(),
                secret_headers: BTreeMap::from([("authorization".into(), "missing".into())]),
            },
            protocol_profile: Default::default(),
            capabilities: Default::default(),
            timeouts: Default::default(),
        });
        registry.validate().unwrap();
        std::fs::write(
            store.path.join("servers.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        assert!(matches!(store.load_snapshot(), Err(Error::Auth)));
    }

    #[test]
    fn snapshot_refuses_either_unfinished_replacement_even_when_documents_are_absent() {
        for name in ["servers.json.tmp", "secrets.json.tmp"] {
            let (_dir, store) = fixture();
            symlink("missing-fixture", store.path.join(name)).unwrap();
            assert_eq!(
                store.load_snapshot().err(),
                Some(Error::RecoveryRequired),
                "{name}"
            );
            assert!(
                std::fs::symlink_metadata(store.path.join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn snapshot_obeys_existing_owner_lock_and_refuses_replaced_store_directory() {
        let (home, store) = fixture();
        store.save_registry(0, &empty(1)).unwrap();
        let lock = store.writer_lock().unwrap();
        assert!(matches!(
            store.load_snapshot(),
            Err(Error::RevisionConflict)
        ));
        drop(lock);
        assert_eq!(store.load_snapshot().unwrap().registry, Some(empty(1)));
        std::fs::rename(&store.path, home.path().join("retired-mcp")).unwrap();
        std::fs::create_dir(&store.path).unwrap();
        std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(store.load_snapshot(), Err(Error::Protection)));
    }

    #[test]
    fn snapshot_protects_both_inodes_and_rejects_corrupt_secrets_with_no_registry() {
        let (_dir, store) = fixture();
        store
            .save_secrets(0, &Secrets::new(1, BTreeMap::new()).unwrap())
            .unwrap();
        let secret = std::fs::metadata(store.path.join("secrets.json")).unwrap();
        store.save_registry(0, &empty(1)).unwrap();
        let registry = std::fs::metadata(store.path.join("servers.json")).unwrap();
        store.load_snapshot().unwrap();
        polaris_auth::protection::with_protected_paths(|snapshot| {
            for metadata in [&secret, &registry] {
                assert!(snapshot.identities().contains(
                    &polaris_auth::protection::ProtectedIdentity {
                        dev: metadata.dev(),
                        ino: metadata.ino(),
                    }
                ));
            }
        })
        .unwrap();
        let (_dir, orphan) = fixture();
        orphan
            .save_secrets(0, &Secrets::new(1, BTreeMap::new()).unwrap())
            .unwrap();
        std::fs::write(orphan.path.join("secrets.json"), b"not JSON").unwrap();
        assert!(matches!(orphan.load_snapshot(), Err(Error::Storage)));
    }

    #[test]
    fn requires_existing_references_and_preserves_referenced_secrets() {
        let (_dir, store) = fixture();
        let secrets = Secrets::new(
            1,
            BTreeMap::from([("credential".into(), "synthetic-secret".into())]),
        )
        .unwrap();
        store.save_secrets(0, &secrets).unwrap();
        let mut registry = empty(1);
        registry.servers.push(Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::new(),
            transport: Destination::StreamableHttp {
                url: "https://example.com/mcp".into(),
                secret_headers: BTreeMap::from([(
                    "authorization".into(),
                    "synthetic-secret".into(),
                )]),
            },
            protocol_profile: ProtocolProfile::default(),
            capabilities: Capabilities::default(),
            timeouts: Timeouts::default(),
        });
        assert_eq!(store.save_registry(0, &registry), Err(Error::Auth));
        assert!(!store.path.join("servers.json").exists());
        if let Destination::StreamableHttp { secret_headers, .. } =
            &mut registry.servers[0].transport
        {
            secret_headers.insert("authorization".into(), "credential".into());
        }
        store.save_registry(0, &registry).unwrap();
        assert_eq!(store.load_registry().unwrap().unwrap(), registry);
        assert!(
            !String::from_utf8(std::fs::read(store.path.join("servers.json")).unwrap())
                .unwrap()
                .contains("synthetic-secret")
        );
        assert_eq!(
            store.save_secrets(1, &Secrets::new(2, BTreeMap::new()).unwrap()),
            Err(Error::Auth)
        );
        assert_eq!(store.load_secrets().unwrap().unwrap().revision(), 1);
        std::fs::write(
            store.path.join("secrets.json"),
            br#"{"schema_version":1,"revision":2,"values":{}}"#,
        )
        .unwrap();
        assert_eq!(store.load_registry(), Err(Error::Auth));
    }

    #[test]
    fn cas_rotates_and_retains_old_inode_protection() {
        let (_dir, store) = fixture();
        assert!(store.load_registry().unwrap().is_none());
        store.save_registry(0, &empty(1)).unwrap();
        let first = std::fs::metadata(store.path.join("servers.json")).unwrap();
        store.save_registry(1, &empty(2)).unwrap();
        assert_eq!(store.load_registry().unwrap().unwrap(), empty(2));
        assert_eq!(
            store.save_registry(1, &empty(2)),
            Err(Error::RevisionConflict)
        );
        assert_eq!(
            store.save_registry(u64::MAX, &empty(1)),
            Err(Error::RevisionConflict)
        );
        polaris_auth::protection::with_protected_paths(|snapshot| {
            assert!(
                snapshot
                    .identities()
                    .contains(&polaris_auth::protection::ProtectedIdentity {
                        dev: first.dev(),
                        ino: first.ino()
                    })
            );
            assert!(
                snapshot
                    .paths()
                    .contains(&store.path.join("servers.json.tmp"))
            );
        })
        .unwrap();
    }

    #[test]
    fn catalog_entries_are_merged_protected_and_never_trusted_after_interruption() {
        use crate::catalog::index::tests::{catalog, server};
        let (_dir, store) = fixture();
        assert!(store.load_catalog_index().unwrap().is_none());
        let fixture = server("https://example.com/mcp");
        let mut other = server("https://other.example.com/mcp");
        other.server_id = "other".into();
        let registry = Registry {
            schema_version: 1,
            revision: 1,
            servers: vec![fixture.clone(), other.clone()],
        };
        let entry = |server: &Server| ServerIndex::from_catalog(server, &catalog(2)).unwrap();
        store.save_registry(0, &registry).unwrap();
        store
            .save_catalog_entry("fixture", entry(&fixture))
            .unwrap();
        store.save_catalog_entry("other", entry(&other)).unwrap();
        let loaded = store.load_catalog_index().unwrap().unwrap();
        assert_eq!(loaded.verified(&registry).len(), 2);
        let path = store.path.join(CATALOG);
        let identity = std::fs::metadata(&path).unwrap();
        polaris_auth::protection::with_protected_paths(|snapshot| {
            assert!(snapshot.paths().contains(&path));
            assert!(
                snapshot
                    .identities()
                    .contains(&polaris_auth::protection::ProtectedIdentity {
                        dev: identity.dev(),
                        ino: identity.ino(),
                    })
            );
        })
        .unwrap();

        std::fs::write(store.path.join("catalog.json.tmp"), b"interrupted").unwrap();
        assert_eq!(
            store.load_catalog_index().err(),
            Some(Error::RecoveryRequired)
        );
        let removed = Registry {
            revision: 2,
            servers: vec![fixture.clone()],
            ..registry.clone()
        };
        store.save_registry(1, &removed).unwrap();
        store
            .save_catalog_entry("fixture", entry(&fixture))
            .unwrap();
        assert!(!store.path.join("catalog.json.tmp").exists());
        let loaded = store.load_catalog_index().unwrap().unwrap();
        assert_eq!(loaded.servers.keys().collect::<Vec<_>>(), ["fixture"]);

        std::fs::write(&path, b"not JSON").unwrap();
        assert_eq!(store.load_catalog_index().err(), Some(Error::Storage));
        assert_eq!(
            store
                .save_catalog_entry("fixture", entry(&fixture))
                .unwrap(),
            (2, true)
        );
        assert_eq!(
            store
                .load_catalog_index()
                .unwrap()
                .unwrap()
                .verified(&removed)["fixture"]
                .tools
                .len(),
            2
        );
        let _lock = store.writer_lock().unwrap();
        assert_eq!(
            store.load_catalog_index().err(),
            Some(Error::RevisionConflict)
        );
    }

    #[test]
    fn catalog_entries_keep_other_owners_servers_and_cut_overflow_as_partial() {
        use crate::catalog::index::tests::{catalog, server};
        let (_dir, store) = fixture();
        let servers: Vec<_> = (0..14)
            .map(|i| {
                let mut server = server(&format!("https://s{i}.example.com/mcp"));
                server.server_id = format!("s{i}");
                server
            })
            .collect();
        store
            .save_registry(
                0,
                &Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: servers[..1].to_vec(),
                },
            )
            .unwrap();
        store
            .save_catalog_entry(
                "s0",
                ServerIndex::from_catalog(&servers[0], &catalog(2)).unwrap(),
            )
            .unwrap();
        // Another owner registers more servers after this check began.
        let registry = Registry {
            schema_version: 1,
            revision: 2,
            servers: servers.clone(),
        };
        store.save_registry(1, &registry).unwrap();
        let mut last = (0, false);
        for server in &servers[1..] {
            let mut entry = ServerIndex::from_catalog(server, &catalog(512)).unwrap();
            entry.partial = false;
            for tool in &mut entry.tools {
                tool.description = "説".repeat(160);
                tool.summary_hash = crate::catalog::index::summary_hash(
                    &server.server_id,
                    &tool.key,
                    &tool.name,
                    &tool.description,
                )
                .unwrap();
            }
            last = store.save_catalog_entry(&server.server_id, entry).unwrap();
        }
        assert!(last.1 && last.0 < 512);
        let index = store.load_catalog_index().unwrap().unwrap();
        assert!(
            std::fs::metadata(store.path.join(CATALOG)).unwrap().len() <= MAX_INDEX_BYTES as u64
        );
        let verified = index.verified(&registry);
        assert!(verified.contains_key("s0") && verified.contains_key("s1"));
        assert_eq!(verified["s13"].tools.len(), last.0);
    }

    #[test]
    fn one_secret_update_uses_cas_and_keeps_referenced_values() {
        let (_dir, store) = fixture();
        store
            .update_secret(0, "credential", Some(SecretValue::new("synthetic-a")))
            .unwrap();
        store
            .update_secret(1, "other", Some(SecretValue::new("synthetic-b")))
            .unwrap();
        let secrets = store.load_secrets().unwrap().unwrap();
        assert_eq!(secrets.revision(), 2);
        assert_eq!(
            secrets.references().collect::<Vec<_>>(),
            ["credential", "other"]
        );
        assert_eq!(
            store.update_secret(1, "other", None),
            Err(Error::RevisionConflict)
        );
        let mut registry = empty(1);
        registry.servers.push(Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::new(),
            transport: Destination::StreamableHttp {
                url: "https://example.com/mcp".into(),
                secret_headers: BTreeMap::from([("authorization".into(), "credential".into())]),
            },
            protocol_profile: ProtocolProfile::default(),
            capabilities: Capabilities::default(),
            timeouts: Timeouts::default(),
        });
        store.save_registry(0, &registry).unwrap();
        assert_eq!(store.update_secret(2, "credential", None), Err(Error::Auth));
        assert_eq!(store.update_secret(2, "missing", None), Err(Error::Auth));
        assert_eq!(
            store.update_secret(2, "bad id", Some(SecretValue::new("synthetic"))),
            Err(Error::Auth)
        );
        store.update_secret(2, "other", None).unwrap();
        let secrets = store.load_secrets().unwrap().unwrap();
        assert_eq!(secrets.resolve("credential").unwrap(), "synthetic-a");
        assert!(secrets.resolve("other").is_err());
    }

    #[test]
    fn a_briefly_held_writer_lock_is_awaited() {
        let (_dir, store) = fixture();
        let held = store.writer_lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(held);
        });
        store.save_registry(0, &empty(1)).unwrap();
        release.join().unwrap();
    }

    #[test]
    fn batched_secrets_share_one_revision_and_server_directories_stay_private() {
        let (dir, store) = fixture();
        store
            .update_secrets(
                0,
                vec![
                    ("a".into(), Some(SecretValue::new("synthetic-a"))),
                    ("b".into(), Some(SecretValue::new("synthetic-b"))),
                ],
            )
            .unwrap();
        assert_eq!(store.load_secrets().unwrap().unwrap().revision(), 1);
        assert_eq!(
            store.update_secrets(1, vec![("a".into(), None), ("missing".into(), None)]),
            Err(Error::Auth)
        );
        assert_eq!(store.load_secrets().unwrap().unwrap().revision(), 1);
        let cwd = store.server_directory("docs-0123456789abcdef").unwrap();
        assert_eq!(
            cwd,
            dir.path()
                .canonicalize()
                .unwrap()
                .join(".polaris/mcp/servers/docs-0123456789abcdef")
        );
        assert_eq!(cwd.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            store.server_directory("docs-0123456789abcdef").unwrap(),
            cwd
        );
        assert_eq!(
            store.server_directory("../escape"),
            Err(Error::InvalidRegistration)
        );
        std::fs::remove_dir(&cwd).unwrap();
        symlink(dir.path(), &cwd).unwrap();
        assert_eq!(
            store.server_directory("docs-0123456789abcdef"),
            Err(Error::Protection)
        );
    }

    #[test]
    fn secrets_round_trip_without_entering_registry() {
        let (_dir, store) = fixture();
        let secrets = Secrets::new(
            1,
            BTreeMap::from([("credential".into(), "synthetic-secret".into())]),
        )
        .unwrap();
        store.save_secrets(0, &secrets).unwrap();
        assert_eq!(
            store
                .load_secrets()
                .unwrap()
                .unwrap()
                .resolve("credential")
                .unwrap(),
            "synthetic-secret"
        );
        assert!(store.load_registry().unwrap().is_none());
    }

    #[test]
    fn rejects_permissive_modes_and_hardlinks_without_overwrite() {
        let (_dir, store) = fixture();
        store.save_registry(0, &empty(1)).unwrap();
        let path = store.path.join("servers.json");
        let original = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.load_registry(), Err(Error::Protection));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&path, store.path.join("alias")).unwrap();
        assert_eq!(store.save_registry(1, &empty(2)), Err(Error::Protection));
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn rejects_symlink_store_and_abandoned_temporary_file() {
        let (_dir, store) = fixture();
        std::fs::write(store.path.join("target"), b"untouched").unwrap();
        symlink(store.path.join("target"), store.path.join("servers.json")).unwrap();
        assert_eq!(store.save_registry(0, &empty(1)), Err(Error::Protection));
        assert_eq!(
            std::fs::read(store.path.join("target")).unwrap(),
            b"untouched"
        );
        let (_dir2, store2) = fixture();
        std::fs::write(store2.path.join("servers.json.tmp"), b"interrupted").unwrap();
        assert_eq!(
            store2.save_registry(0, &empty(1)),
            Err(Error::RecoveryRequired)
        );
        assert_eq!(
            std::fs::read(store2.path.join("servers.json.tmp")).unwrap(),
            b"interrupted"
        );
    }

    #[test]
    fn serializes_separate_owners_and_rejects_directory_replacement() {
        let (dir, store) = fixture();
        let other = Store::open(&dir.path().canonicalize().unwrap()).unwrap();
        let guard = store.writer_lock().unwrap();
        assert_eq!(
            other.save_registry(0, &empty(1)),
            Err(Error::RevisionConflict)
        );
        drop(guard);
        other.save_registry(0, &empty(1)).unwrap();
        std::fs::rename(&store.path, store.path.with_file_name("old-mcp")).unwrap();
        std::fs::create_dir(&store.path).unwrap();
        assert_eq!(store.save_registry(1, &empty(2)), Err(Error::Protection));
    }

    #[test]
    fn permits_existing_non_writable_parent_and_rejects_parent_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir(home.join(".polaris")).unwrap();
        std::fs::set_permissions(
            home.join(".polaris"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let store = Store::open(&home).unwrap();
        store.save_registry(0, &empty(1)).unwrap();
        assert_eq!(
            std::fs::metadata(home.join(".polaris")).unwrap().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&store.path).unwrap().mode() & 0o777,
            0o700
        );
        std::fs::rename(home.join(".polaris"), home.join("moved")).unwrap();
        symlink(home.join("moved"), home.join(".polaris")).unwrap();
        assert_eq!(store.save_registry(1, &empty(2)), Err(Error::Protection));
    }

    #[test]
    fn refuses_oversize_and_unknown_fields_without_partial_replacement() {
        let (_dir, store) = fixture();
        let large = Secrets::new(
            1,
            BTreeMap::from([("credential".into(), "s".repeat(MAX_STORE_BYTES))]),
        )
        .unwrap();
        assert_eq!(store.save_secrets(0, &large), Err(Error::Limit));
        assert!(!store.path.join("secrets.json.tmp").exists());
        store.save_registry(0, &empty(1)).unwrap();
        std::fs::write(
            store.path.join("servers.json"),
            br#"{"schema_version":1,"revision":1,"servers":[],"unknown":true}"#,
        )
        .unwrap();
        assert_eq!(store.load_registry(), Err(Error::Storage));
    }
}

#[cfg(test)]
mod portable_tests {
    use super::*;

    #[test]
    fn failed_inode_registration_prevents_first_secret_byte() {
        let mut file = tempfile::tempfile().unwrap();
        let result = write_protected(&mut file, b"synthetic-secret", |file| {
            assert_eq!(file.metadata().unwrap().len(), 0);
            Err(Error::Protection)
        });
        assert_eq!(result, Err(Error::Protection));
        assert_eq!(file.metadata().unwrap().len(), 0);
        write_protected(&mut file, b"synthetic-secret", |_| Ok(())).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 16);
    }

    #[test]
    fn secret_values_validate_in_memory_and_debug_stays_redacted() {
        let secrets = Secrets::new(
            7,
            BTreeMap::from([("credential".into(), "synthetic-value".into())]),
        )
        .unwrap();
        assert_eq!(secrets.revision(), 7);
        assert_eq!(secrets.references().collect::<Vec<_>>(), vec!["credential"]);
        assert_eq!(secrets.resolve("credential"), Ok("synthetic-value"));
        assert_eq!(secrets.resolve("missing"), Err(Error::Auth));
        assert_eq!(
            format!("{:?}", SecretValue::new("synthetic-value")),
            "SecretValue(<redacted>)"
        );
        assert!(Secrets::new(0, BTreeMap::new()).is_err());
        assert!(Secrets::new(1, BTreeMap::from([("bad id".into(), "value".into())])).is_err());
        assert_eq!(format!("{}", Error::Auth), "mcp_auth");
        assert!(Secrets::new(2, BTreeMap::from([("credential".into(), "a\r\nb".into())])).is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;

    fn contents(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            assert!(metadata.is_file() || metadata.is_dir());
            entries.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                metadata.is_file().then(|| fs::read(path).unwrap()),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(root, &entry.unwrap().path(), entries);
                }
            }
        }
        let mut entries = BTreeMap::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn windows_mcp_store_unavailable_open_preserves_existing_and_missing_paths() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(home.join(".polaris/mcp")).unwrap();
        fs::write(
            home.join(".polaris/mcp/servers.json"),
            b"invalid fixture JSON",
        )
        .unwrap();
        fs::write(root.path().join("file"), b"fixture file").unwrap();
        let before = contents(root.path());
        for path in [
            home,
            root.path().to_path_buf(),
            root.path().join("missing"),
            root.path().join("file"),
            PathBuf::from("relative-fixture-home"),
        ] {
            assert!(matches!(Store::open(&path), Err(Error::Protection)));
            assert_eq!(contents(root.path()), before);
        }
    }

    #[test]
    fn windows_mcp_store_unavailable_methods_refuse_without_reading_or_writing() {
        let root = tempfile::tempdir().unwrap();
        let home_file = root.path().join("home-handle");
        let directory_file = root.path().join("directory-handle");
        fs::write(&home_file, b"home fixture").unwrap();
        fs::write(&directory_file, b"directory fixture").unwrap();
        let path = root.path().join("store");
        fs::create_dir(&path).unwrap();
        for name in ["servers.json", "secrets.json", "catalog.json", "owner.lock"] {
            fs::write(path.join(name), b"not a valid stored document").unwrap();
        }
        // Only this private test assembles handles. It does not acquire a
        // protected Windows owner; public construction must remain unavailable.
        let store = Store {
            home: File::open(home_file).unwrap(),
            directory: File::open(directory_file).unwrap(),
            path,
        };
        let registry = Registry {
            schema_version: 1,
            revision: 1,
            servers: vec![],
        };
        let secrets = Secrets::new(1, BTreeMap::new()).unwrap();
        let before = contents(root.path());
        assert!(matches!(store.load_registry(), Err(Error::Protection)));
        assert!(matches!(store.load_secrets(), Err(Error::Protection)));
        assert!(matches!(store.load_snapshot(), Err(Error::Protection)));
        assert_eq!(store.save_registry(0, &registry), Err(Error::Protection));
        assert_eq!(store.save_secrets(0, &secrets), Err(Error::Protection));
        assert_eq!(
            store.update_secret(0, "credential", Some(SecretValue::new("synthetic-value"))),
            Err(Error::Protection)
        );
        assert_eq!(
            store.update_secrets(0, vec![("credential".into(), None)]),
            Err(Error::Protection)
        );
        assert_eq!(store.server_directory("fixture"), Err(Error::Protection));
        assert!(matches!(store.load_catalog_index(), Err(Error::Protection)));
        assert_eq!(
            store.save_catalog_entry(
                "fixture",
                ServerIndex {
                    destination_hash: "fixture".into(),
                    partial: false,
                    tools: vec![],
                }
            ),
            Err(Error::Protection)
        );
        assert_eq!(contents(root.path()), before);
    }

    #[test]
    fn windows_mcp_store_metadata_presence_is_observed_without_creation() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        fs::create_dir_all(existing.join(".polaris/mcp")).unwrap();
        let file = root.path().join("file");
        fs::create_dir_all(file.join(".polaris")).unwrap();
        fs::write(file.join(".polaris/mcp"), b"not a directory").unwrap();
        let before = contents(root.path());
        assert!(Store::exists(&existing));
        assert!(!Store::exists(&file));
        assert!(!Store::exists(&root.path().join("missing")));
        assert_eq!(contents(root.path()), before);
    }
}
