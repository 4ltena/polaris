//! Descriptor-relative private storage shared by trusted host surfaces.
//!
//! A stable file lock serializes cooperating processes. The protection lock is
//! held from descriptor inspection until durable replacement; paths and retired
//! inodes remain excluded from model-visible snapshots.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use polaris_auth::protection::protect_store;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::{Zeroize, Zeroizing};

use crate::Error;
use crate::catalog::index::{CatalogIndex, MAX_INDEX_BYTES, ServerIndex};
use crate::registration::{Destination, MAX_STORE_BYTES, Registry, bounded_json, valid_id};

const CATALOG: &str = "catalog.json";

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
    values: BTreeMap<String, String>,
}

impl Drop for SecretDocument {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
    }
}

impl Secrets {
    pub fn new(revision: u64, values: BTreeMap<String, String>) -> Result<Self, Error> {
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
            .map(String::as_str)
            .ok_or(Error::Auth)
    }
}

trait Document: Serialize + DeserializeOwned {
    fn revision(&self) -> u64;
    fn validate(&self) -> Result<(), Error>;
    fn validate_links(
        &self,
        store: &Store,
        guard: &mut polaris_auth::protection::StoreProtectionGuard,
    ) -> Result<(), Error>;
}

impl Document for Registry {
    fn revision(&self) -> u64 {
        self.revision
    }
    fn validate(&self) -> Result<(), Error> {
        Registry::validate(self)
    }
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

pub struct Store {
    home: File,
    directory: File,
    path: PathBuf,
}

struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn write_protected(
    file: &mut File,
    bytes: &[u8],
    register: impl FnOnce(&File) -> Result<(), Error>,
) -> Result<(), Error> {
    register(file)?;
    file.write_all(bytes).map_err(|_| Error::Storage)?;
    file.sync_all().map_err(|_| Error::Storage)
}

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

fn open_at(parent: &File, name: &str, flags: i32) -> Result<File, std::io::Error> {
    let name = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
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

fn parent_metadata(file: &File) -> Result<(), Error> {
    let m = file.metadata().map_err(|_| Error::Storage)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(Error::Protection);
    }
    Ok(())
}

impl Store {
    /// Whether a store directory was ever created, without creating it.
    pub fn exists(home: &Path) -> bool {
        home.join(".polaris/mcp")
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_dir())
    }

    /// `home` must be the trusted OS-resolved canonical home, never project input.
    /// Reserve store paths before calling any model-visible reader.
    pub fn open(home: &Path) -> Result<Self, Error> {
        if !home.is_absolute() || home.canonicalize().map_err(|_| Error::Protection)? != home {
            return Err(Error::Protection);
        }
        let path = home.join(".polaris/mcp");
        // Reserve both stores before creating any file that can contain secrets.
        drop(protect_store(&path.join("servers.json")).map_err(|_| Error::Protection)?);
        drop(protect_store(&path.join("secrets.json")).map_err(|_| Error::Protection)?);
        drop(protect_store(&path.join("owner.lock")).map_err(|_| Error::Protection)?);
        drop(protect_store(&path.join(CATALOG)).map_err(|_| Error::Protection)?);
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(home)
            .map_err(|_| Error::Protection)?;
        let m = root.metadata().map_err(|_| Error::Storage)?;
        if m.uid() != unsafe { libc::geteuid() } || !m.is_dir() {
            return Err(Error::Protection);
        }
        let polaris = private_directory(&root, ".polaris", false)?;
        let directory = private_directory(&polaris, "mcp", true)?;
        Ok(Self {
            home: root,
            directory,
            path,
        })
    }

    fn writer_lock(&self) -> Result<WriterLock, Error> {
        // Do not keep the in-process protection lock while awaiting a file lock.
        let file = open_at(&self.directory, "owner.lock", libc::O_RDWR | libc::O_CREAT)
            .map_err(|_| Error::Protection)?;
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

    fn check_directory(&self) -> Result<(), Error> {
        let parent = open_at(&self.home, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| Error::Protection)?;
        parent_metadata(&parent)?;
        let current = open_at(&parent, "mcp", libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| Error::Protection)?;
        metadata(&current, true)?;
        let a = current.metadata().map_err(|_| Error::Storage)?;
        let b = self.directory.metadata().map_err(|_| Error::Storage)?;
        if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
            return Err(Error::Protection);
        }
        Ok(())
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
        let _writer = self.writer_lock()?;
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
                    // This is the persistence boundary. The stored document
                    // deliberately retains the credential until it is removed;
                    // transient trusted input remains zeroizing before here.
                    if let Some(mut replaced) = values.insert(reference, value.0.to_string()) {
                        replaced.zeroize();
                    }
                }
                None => {
                    let mut removed = values.remove(&reference).ok_or(Error::Auth)?;
                    removed.zeroize();
                }
            }
        }
        let next = Secrets::new(
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

#[cfg(test)]
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
        assert_eq!(format!("{}", Error::Auth), "mcp_auth");
        assert!(Secrets::new(2, BTreeMap::from([("credential".into(), "a\r\nb".into())])).is_err());
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
