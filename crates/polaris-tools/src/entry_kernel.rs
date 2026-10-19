//! Shared entry wire and descriptor primitives for trusted host owners.
//! No stop proof, migration policy, intent writer, or activation authority.
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const GATE_FILE: &str = "mcp-entry-gate-v2.json";
pub const GATE_LOCK: &str = "mcp-entry-gate-v2.lock";
pub const ENTRY_MANIFEST: &str = "mcp-entry-identities-v2.json";
pub const MAX_GATE_BYTES: u64 = 64 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EntryGateErrorV2 {
    #[error("mcp_entry_migration_in_progress")]
    Migrating,
    #[error("mcp_entry_busy")]
    Busy,
    #[error("mcp_entry_identity_unavailable")]
    Identity,
    #[error("mcp_entry_gate_invalid")]
    InvalidGate,
    #[error("mcp_entry_storage")]
    Storage,
    #[error("mcp_entry_process_unresolved")]
    ProcessUnresolved,
    #[error("mcp_entry_process_active")]
    ProcessActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryAnchorV2 {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub mode: u32,
    pub links: u64,
}

impl EntryAnchorV2 {
    pub fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            uid: meta.uid(),
            mode: meta.mode() & 0o7777,
            links: meta.nlink(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionBinaryIdentityV2 {
    pub path: PathBuf,
    pub anchor: EntryAnchorV2,
    pub sha256: String,
}

impl ProductionBinaryIdentityV2 {
    pub fn inspect(path: &Path) -> Result<Self, EntryGateErrorV2> {
        let mut file = physical_file(path)?;
        let before = file.metadata().map_err(storage)?;
        if !before.is_file()
            || before.nlink() != 1
            || ![0, unsafe { libc::geteuid() }].contains(&before.uid())
            || before.mode() & 0o7022 != 0
            || before.mode() & 0o111 == 0
            || before.len() == 0
            || before.len() > MAX_BINARY_BYTES
        {
            return Err(EntryGateErrorV2::Identity);
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 32768];
        let mut total = 0u64;
        loop {
            let count = file.read(&mut buffer).map_err(storage)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or(EntryGateErrorV2::Identity)?;
            if total > MAX_BINARY_BYTES {
                return Err(EntryGateErrorV2::Identity);
            }
            hasher.update(&buffer[..count]);
        }
        let after = file.metadata().map_err(storage)?;
        let named = physical_file(path)?.metadata().map_err(storage)?;
        if !stable_binary(&before, &after)
            || !stable_binary(&before, &named)
            || total != before.len()
        {
            return Err(EntryGateErrorV2::Identity);
        }
        Ok(Self {
            path: path.to_owned(),
            anchor: EntryAnchorV2::of(&before),
            sha256: format!("{:x}", hasher.finalize()),
        })
    }

    pub fn verify(&self) -> Result<(), EntryGateErrorV2> {
        if !valid_digest(&self.sha256) || Self::inspect(&self.path)? != *self {
            return Err(EntryGateErrorV2::Identity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionEntryKindV2 {
    Cli,
    DesktopApp,
    DesktopService,
    ExecutionHelper,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionEntryIdentityV2 {
    pub kind: ProductionEntryKindV2,
    pub binary: ProductionBinaryIdentityV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionEntryManifestV2 {
    pub protocol_version: u32,
    pub entries: Vec<ProductionEntryIdentityV2>,
    /// Only a verified private runtime/polaris copy of these exact bundled
    /// helpers can enter as an isolated execution helper.
    pub runtime_copy_source_hashes: Vec<String>,
}

impl ProductionEntryManifestV2 {
    pub fn validate(&self) -> Result<(), EntryGateErrorV2> {
        if self.protocol_version != 2
            || self.entries.is_empty()
            || self.entries.len() > MAX_ENTRIES
            || self.runtime_copy_source_hashes.len() > 2
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let mut paths = BTreeSet::new();
        let mut kinds = BTreeSet::new();
        for entry in &self.entries {
            if !paths.insert(&entry.binary.path) || !kinds.insert(entry.kind) {
                return Err(EntryGateErrorV2::InvalidGate);
            }
            entry.binary.verify()?;
        }
        let mut copy_hashes = BTreeSet::new();
        for hash in &self.runtime_copy_source_hashes {
            if !valid_digest(hash)
                || !copy_hashes.insert(hash)
                || !self.entries.iter().any(|entry| {
                    matches!(
                        entry.kind,
                        ProductionEntryKindV2::ExecutionHelper | ProductionEntryKindV2::Cli
                    ) && entry.binary.sha256 == *hash
                })
            {
                return Err(EntryGateErrorV2::InvalidGate);
            }
        }
        Ok(())
    }

    pub fn canonical_hash(&self) -> Result<String, EntryGateErrorV2> {
        self.validate()?;
        canonical_hash(self)
    }

    pub fn admits(&self, current: &ProductionBinaryIdentityV2) -> Result<(), EntryGateErrorV2> {
        if self.entries.iter().any(|entry| &entry.binary == current) {
            return Ok(());
        }
        if self.runtime_copy_source_hashes.contains(&current.sha256) {
            validate_runtime_copy(current)?;
            return Ok(());
        }
        Err(EntryGateErrorV2::Identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductionEntryGateV2 {
    MigratingV2 {
        protocol_version: u32,
        migration_id: String,
        entry_manifest_hash: String,
    },
    ActiveV2 {
        protocol_version: u32,
        migration_id: String,
        entry_manifest_hash: String,
        active_intent_hash: String,
    },
}

impl ProductionEntryGateV2 {
    pub fn validate(&self) -> Result<(), EntryGateErrorV2> {
        let (version, migration, manifest, intent) = match self {
            Self::MigratingV2 {
                protocol_version,
                migration_id,
                entry_manifest_hash,
            } => (*protocol_version, migration_id, entry_manifest_hash, None),
            Self::ActiveV2 {
                protocol_version,
                migration_id,
                entry_manifest_hash,
                active_intent_hash,
            } => (
                *protocol_version,
                migration_id,
                entry_manifest_hash,
                Some(active_intent_hash),
            ),
        };
        if version != 2
            || !valid_digest(migration)
            || !valid_digest(manifest)
            || intent.is_some_and(|value| !valid_digest(value))
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        Ok(())
    }
}

pub fn validate_runtime_copy(
    identity: &ProductionBinaryIdentityV2,
) -> Result<(), EntryGateErrorV2> {
    runtime_copy_anchors(identity).map(|_| ())
}

pub fn runtime_copy_anchors(
    identity: &ProductionBinaryIdentityV2,
) -> Result<(EntryAnchorV2, EntryAnchorV2), EntryGateErrorV2> {
    let path = &identity.path;
    if path.file_name().and_then(|name| name.to_str()) != Some("polaris") {
        return Err(EntryGateErrorV2::Identity);
    }
    let runtime = path.parent().ok_or(EntryGateErrorV2::Identity)?;
    if runtime.file_name().and_then(|name| name.to_str()) != Some("runtime") {
        return Err(EntryGateErrorV2::Identity);
    }
    let run = runtime.parent().ok_or(EntryGateErrorV2::Identity)?;
    let runtime_fd = physical_search_directory(runtime)?;
    let run_fd = physical_search_directory(run)?;
    for (fd, mode) in [(&runtime_fd, 0o500), (&run_fd, 0o700)] {
        let meta = fd.metadata().map_err(storage)?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != mode {
            return Err(EntryGateErrorV2::Identity);
        }
    }
    if identity.anchor.uid != unsafe { libc::geteuid() } || identity.anchor.mode != 0o500 {
        return Err(EntryGateErrorV2::Identity);
    }
    // Descriptor traversal, private anchors and source digest together are the
    // admission proof. A basename alone never establishes helper identity.
    Ok((
        EntryAnchorV2::of(&runtime_fd.metadata().map_err(storage)?),
        EntryAnchorV2::of(&run_fd.metadata().map_err(storage)?),
    ))
}

pub fn stable_binary(a: &Metadata, b: &Metadata) -> bool {
    EntryAnchorV2::of(a) == EntryAnchorV2::of(b)
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
pub fn canonical_hash<T: Serialize>(value: &T) -> Result<String, EntryGateErrorV2> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|_| EntryGateErrorV2::InvalidGate)?)
    ))
}
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, EntryGateErrorV2> {
    serde_json::from_slice(bytes).map_err(|_| EntryGateErrorV2::InvalidGate)
}
fn storage(_: std::io::Error) -> EntryGateErrorV2 {
    EntryGateErrorV2::Storage
}

pub fn physical_directory(path: &Path) -> Result<File, EntryGateErrorV2> {
    let parts = components(path)?;
    let mut directory = File::open("/").map_err(storage)?;
    for part in parts {
        directory = child(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| EntryGateErrorV2::Identity)?;
    }
    Ok(directory)
}
pub fn physical_file(path: &Path) -> Result<File, EntryGateErrorV2> {
    let name = path.file_name().ok_or(EntryGateErrorV2::Identity)?;
    let directory = physical_search_directory(path.parent().ok_or(EntryGateErrorV2::Identity)?)?;
    child_bytes(&directory, name.as_bytes(), libc::O_RDONLY).map_err(|_| EntryGateErrorV2::Identity)
}

/// A no-follow directory capability for metadata and relative opens only.
/// Migration enumeration and fsync must keep using `physical_directory`.
pub fn physical_search_directory(path: &Path) -> Result<File, EntryGateErrorV2> {
    let parts = components(path)?;
    let mut directory = crate::account_home::open_search_root().map_err(storage)?;
    for part in parts {
        directory = search_directory(&directory, part).map_err(|_| EntryGateErrorV2::Identity)?;
    }
    Ok(directory)
}

pub fn search_directory(parent: &File, name: &str) -> std::io::Result<File> {
    let name = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    crate::account_home::open_search_child(parent, &name)
}
fn components(path: &Path) -> Result<Vec<&str>, EntryGateErrorV2> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().len() > 4096
        || path.as_os_str().as_bytes().contains(&0)
        || path == Path::new("/")
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(EntryGateErrorV2::Identity);
    }
    // Reject textual alias components which Path::components would normalize.
    let text = path.to_str().ok_or(EntryGateErrorV2::Identity)?;
    let parts: Vec<_> = text[1..].split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(EntryGateErrorV2::Identity);
    }
    Ok(parts)
}
pub fn child(parent: &File, name: &str, flags: i32) -> Result<File, std::io::Error> {
    child_bytes(parent, name.as_bytes(), flags)
}
pub fn child_bytes(parent: &File, name: &[u8], flags: i32) -> Result<File, std::io::Error> {
    let name = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
pub fn owned_directory(file: &File, private: bool) -> Result<(), EntryGateErrorV2> {
    let meta = file.metadata().map_err(storage)?;
    if !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & (if private { 0o077 } else { 0o022 }) != 0
    {
        return Err(EntryGateErrorV2::Identity);
    }
    Ok(())
}
pub fn private_file(file: &File) -> Result<(), EntryGateErrorV2> {
    let meta = file.metadata().map_err(storage)?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
    {
        return Err(EntryGateErrorV2::InvalidGate);
    }
    Ok(())
}
pub fn read_optional(parent: &File, name: &str) -> Result<Option<Vec<u8>>, EntryGateErrorV2> {
    let mut file = match child(parent, name, libc::O_RDONLY) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => result.map_err(storage)?,
    };
    private_file(&file)?;
    let before = file.metadata().map_err(storage)?;
    if before.len() > MAX_GATE_BYTES {
        return Err(EntryGateErrorV2::InvalidGate);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_GATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(storage)?;
    let after = file.metadata().map_err(storage)?;
    let named = child(parent, name, libc::O_RDONLY)
        .map_err(storage)?
        .metadata()
        .map_err(storage)?;
    if bytes.len() as u64 != before.len()
        || !stable_binary(&before, &after)
        || !stable_binary(&before, &named)
    {
        return Err(EntryGateErrorV2::InvalidGate);
    }
    Ok(Some(bytes))
}

/// Nonblocking lock primitives on the same validated physical gate lock.
pub fn lock_shared(file: &File) -> Result<(), EntryGateErrorV2> {
    lock(file, libc::LOCK_SH)
}
pub fn lock_exclusive(file: &File) -> Result<(), EntryGateErrorV2> {
    lock(file, libc::LOCK_EX)
}
fn lock(file: &File, mode: i32) -> Result<(), EntryGateErrorV2> {
    private_file(file)?;
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
        return Err(EntryGateErrorV2::Busy);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn search_directory_retains_metadata_and_relative_opens() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("leaf"), b"owned leaf").unwrap();
        let directory = physical_search_directory(root.path()).unwrap();
        assert!(directory.metadata().unwrap().is_dir());
        let leaf = child(&directory, "leaf", libc::O_RDONLY).unwrap();
        assert_eq!(leaf.metadata().unwrap().len(), 10);
        // SAFETY: F_GETFL inspects the retained descriptor without mutation.
        let flags = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETFL) };
        #[cfg(target_os = "macos")]
        // O_DIRECTORY validates open-time type and is not retained by F_GETFL.
        assert_eq!(flags & libc::O_EXEC, libc::O_EXEC);
        #[cfg(target_os = "linux")]
        assert_eq!(flags & libc::O_PATH, libc::O_PATH);
        #[cfg(target_os = "linux")]
        {
            // SAFETY: dup creates a separate owned FD. Some libc versions wrap
            // O_PATH in DIR; neither that nor outright rejection may enumerate.
            let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
            assert!(duplicate >= 0);
            unsafe { *libc::__errno_location() = 0 };
            let stream = unsafe { libc::fdopendir(duplicate) };
            if stream.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe { libc::close(duplicate) };
                assert_eq!(error.raw_os_error(), Some(libc::EBADF));
            } else {
                unsafe { *libc::__errno_location() = 0 };
                let entry = unsafe { libc::readdir(stream) };
                let error = std::io::Error::last_os_error();
                unsafe { libc::closedir(stream) };
                assert!(entry.is_null(), "search-only descriptor must not enumerate");
                assert_eq!(error.raw_os_error(), Some(libc::EBADF));
            }
        }
        for name in ["", ".", "..", "a/b"] {
            assert!(search_directory(&directory, name).is_err());
        }
        std::os::unix::fs::symlink(root.path(), root.path().join("alias")).unwrap();
        assert!(search_directory(&directory, "alias").is_err());
        assert!(search_directory(&directory, "leaf").is_err());
        // Readable migration directories still support enumeration and fsync.
        let readable = physical_directory(root.path()).unwrap();
        readable.sync_all().unwrap();
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
    #[test]
    fn shared_entry_kernel_wire_matches_independent_canonical_fixture() {
        let bytes = br#"{"state":"migrating_v2","protocol_version":2,"migration_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","entry_manifest_hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;
        let gate: ProductionEntryGateV2 = decode(bytes).unwrap();
        gate.validate().unwrap();
        assert_eq!(serde_json::to_vec(&gate).unwrap(), bytes);
        let mut unknown: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        unknown["unknown"] = true.into();
        assert!(decode::<ProductionEntryGateV2>(&serde_json::to_vec(&unknown).unwrap()).is_err());
        unknown.as_object_mut().unwrap().remove("unknown");
        unknown["migration_id"] = "bad".into();
        assert!(
            decode::<ProductionEntryGateV2>(&serde_json::to_vec(&unknown).unwrap())
                .unwrap()
                .validate()
                .is_err()
        );
    }
    #[test]
    fn shared_entry_kernel_locks_use_same_physical_file_in_both_orders() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(GATE_LOCK);
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let parent = physical_directory(root.path()).unwrap();
        for shared_first in [true, false] {
            let first = child(&parent, GATE_LOCK, libc::O_RDONLY).unwrap();
            let second = child(&parent, GATE_LOCK, libc::O_RDONLY).unwrap();
            assert_eq!(
                EntryAnchorV2::of(&first.metadata().unwrap()),
                EntryAnchorV2::of(&second.metadata().unwrap())
            );
            if shared_first {
                lock_shared(&first).unwrap();
                assert!(matches!(
                    lock_exclusive(&second),
                    Err(EntryGateErrorV2::Busy)
                ));
            } else {
                lock_exclusive(&first).unwrap();
                assert!(matches!(lock_shared(&second), Err(EntryGateErrorV2::Busy)));
            }
            drop(first);
            if shared_first {
                lock_exclusive(&second).unwrap();
            } else {
                lock_shared(&second).unwrap();
            }
        }
        assert_eq!(fs::read(&path).unwrap(), b"");
    }
}

#[cfg(target_os = "macos")]
mod package;
