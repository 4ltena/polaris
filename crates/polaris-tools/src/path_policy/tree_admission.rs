//! Shared descriptor-based tree inspection for sanitized copies and strict
//! registered-directory admission. Destination writes are optional; security
//! classification, protected identities, limits and race checks are identical.
#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr};
#[cfg(unix)]
use std::fs::{self, File, Metadata, Permissions};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
mod registered;
#[cfg(unix)]
pub use registered::RegisteredDirectory;

/// Every bound is mandatory; zero permits no corresponding resource.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_entries: usize,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_depth: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Identity {
    pub device: u64,
    pub inode: u64,
}

#[cfg(unix)]
impl Identity {
    pub fn of(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ObservedTreeEntry {
    pub relative_path: PathBuf,
    pub identity: Identity,
    pub bytes: u64,
    /// Only ordinary rwx bits; setid and sticky bits are never preserved.
    pub mode: u32,
    /// None for directories.
    pub sha256: Option<[u8; 32]>,
    // Preserve special permission bits for source conflict detection only.
    // They must never be applied to the output workspace.
    pub observed_mode: u32,
    pub observed_mtime: (i64, i64),
    pub observed_ctime: (i64, i64),
}

impl ObservedTreeEntry {
    /// Original source permission bits for approval/conflict evidence only.
    /// Use `mode` for installing output; special bits must not be restored.
    pub fn observed_mode(&self) -> u32 {
        self.observed_mode
    }

    pub fn observed_mtime(&self) -> (i64, i64) {
        self.observed_mtime
    }

    pub fn observed_ctime(&self) -> (i64, i64) {
        self.observed_ctime
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusionReason {
    SecretName,
    RegisteredSecret,
    Symlink,
    MultipleLinks,
    SpecialFile,
    /// Git metadata, or an ancestor change that would affect protected metadata.
    RepositoryMetadata,
    /// Ephemeral build output deliberately omitted from rewind checkpoints.
    BuildCache,
}

#[derive(Clone, Debug)]
pub struct Exclusion {
    pub relative_path: PathBuf,
    pub reason: ExclusionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    ProtectionUnavailable,
    InvalidPath,
    Io,
    Limit,
    SourceChanged,
    DestinationInsideSource,
    Deadline,
}

/// Diagnostics deliberately retain no input contents or arbitrary error messages.
#[derive(Debug)]
pub struct SnapshotError {
    pub path: PathBuf,
    pub kind: ErrorKind,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.path.display())
    }
}
impl std::error::Error for SnapshotError {}
pub type Result<T> = std::result::Result<T, SnapshotError>;
pub fn error(path: &Path, kind: ErrorKind) -> SnapshotError {
    SnapshotError {
        path: path.into(),
        kind,
    }
}
pub fn io<T>(path: &Path, value: std::io::Result<T>) -> Result<T> {
    value.map_err(|_| error(path, ErrorKind::Io))
}

/// Metadata-only registration; absent paths remain reserved. Existing target
/// identities also exclude single-link files renamed away from registered names.
#[derive(Clone, Debug)]
pub struct RegisteredSecrets {
    paths: Vec<PathBuf>,
    identities: HashSet<Identity>,
}

impl RegisteredSecrets {
    /// Import metadata while the auth owner holds its registration/rotation lock.
    #[cfg(unix)]
    pub fn from_auth_registry(
        registered: &polaris_auth::protection::ProtectedPathsSnapshot,
    ) -> Result<Self> {
        let mut secrets = Self::new(registered.paths().iter().cloned())?;
        secrets
            .identities
            .extend(registered.identities().iter().map(|identity| Identity {
                device: identity.dev,
                inode: identity.ino,
            }));
        Ok(secrets)
    }
    #[cfg(unix)]
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut result = Self {
            paths: Vec::new(),
            identities: HashSet::new(),
        };
        for path in paths {
            validate_absolute(&path)?;
            match fs::metadata(&path) {
                Ok(m) => {
                    result.identities.insert(Identity::of(&m));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(error(&path, ErrorKind::Io)),
            }
            // Resolve existing ancestors using metadata only, including /tmp on
            // macOS. Keep missing leaf components reserved for later creation.
            let mut ancestor = path.as_path();
            let mut suffix = Vec::new();
            loop {
                match fs::canonicalize(ancestor) {
                    Ok(mut physical) => {
                        for name in suffix.iter().rev() {
                            physical.push(name);
                        }
                        result.paths.push(physical);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        suffix.push(
                            ancestor
                                .file_name()
                                .ok_or_else(|| error(&path, ErrorKind::InvalidPath))?,
                        );
                        ancestor = ancestor
                            .parent()
                            .ok_or_else(|| error(&path, ErrorKind::InvalidPath))?;
                    }
                    Err(_) => return Err(error(&path, ErrorKind::Io)),
                }
            }
            result.paths.push(path);
        }
        Ok(result)
    }

    /// Reserve an already physical logical path without opening its contents.
    #[cfg(unix)]
    pub fn reserve_path(&mut self, path: PathBuf) -> Result<()> {
        validate_absolute(&path)?;
        self.paths.push(path);
        Ok(())
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
    pub fn identities(&self) -> &HashSet<Identity> {
        &self.identities
    }
    #[cfg(unix)]
    pub fn refreshed(&self) -> Result<Self> {
        let mut refreshed = Self::new(self.paths.clone())?;
        refreshed.identities.extend(self.identities.iter().copied());
        Ok(refreshed)
    }

    #[cfg(unix)]
    pub fn denies(&self, path: &Path, m: &Metadata) -> bool {
        self.paths.iter().any(|secret| path.starts_with(secret))
            || self.identities.contains(&Identity::of(m))
    }

    pub fn reserves(&self, path: &Path) -> bool {
        self.paths
            .iter()
            .any(|secret| path.starts_with(secret) || secret.starts_with(path))
    }
}

#[cfg(not(unix))]
impl RegisteredSecrets {
    pub fn from_auth_registry(
        _registered: &polaris_auth::protection::ProtectedPathsSnapshot,
    ) -> Result<Self> {
        Err(error(Path::new(""), ErrorKind::ProtectionUnavailable))
    }

    pub fn new(_paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        Err(error(Path::new(""), ErrorKind::ProtectionUnavailable))
    }

    pub fn reserve_path(&mut self, path: PathBuf) -> Result<()> {
        Err(error(&path, ErrorKind::ProtectionUnavailable))
    }

    pub fn refreshed(&self) -> Result<Self> {
        Err(error(Path::new(""), ErrorKind::ProtectionUnavailable))
    }
}

pub fn validate_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(error(path, ErrorKind::InvalidPath));
    }
    Ok(())
}

#[cfg(unix)]
pub fn open_at(parent: &File, name: &OsStr, directory: bool, path: &Path) -> Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| error(path, ErrorKind::InvalidPath))?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: name is NUL terminated, parent is live; ownership transfers once.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(error(path, ErrorKind::Io));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Opens every ancestor without following symlinks (use physical absolute paths).
#[cfg(unix)]
pub fn open_root_with_ancestors(path: &Path) -> Result<(File, Vec<(PathBuf, Identity)>)> {
    validate_absolute(path)?;
    let mut dir = io(path, File::open("/"))?;
    let mut prefix = PathBuf::from("/");
    let mut ancestors = vec![(prefix.clone(), Identity::of(&io(path, dir.metadata())?))];
    for c in path.components() {
        if let Component::Normal(name) = c {
            prefix.push(name);
            dir = open_at(&dir, name, true, &prefix)?;
            ancestors.push((prefix.clone(), Identity::of(&io(&prefix, dir.metadata())?)));
        }
    }
    Ok((dir, ancestors))
}

#[cfg(unix)]
pub fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    Identity::of(a) == Identity::of(b)
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

#[cfg(unix)]
struct Directory(*mut libc::DIR);
#[cfg(unix)]
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(unix)]
pub struct TreeWalker<'a> {
    pub secrets: &'a RegisteredSecrets,
    pub private_owner: Option<u32>,
    pub limits: Limits,
    pub entries: usize,
    pub files: usize,
    pub bytes: u64,
    pub root_path: &'a Path,
    pub policy_root: &'a Path,
    pub destination: Option<&'a Path>,
    pub selection: Option<&'a Selection>,
    pub manifest: Vec<ObservedTreeEntry>,
    pub exclusions: Vec<Exclusion>,
    pub deadline: Option<Instant>,
    pub exclude_build_caches: bool,
}

#[cfg(unix)]
impl TreeWalker<'_> {
    pub fn walk(&mut self, dir: &File, relative: &Path, depth: usize) -> Result<()> {
        let absolute = self.root_path.join(relative);
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(error(&absolute, ErrorKind::Deadline));
        }
        let before = io(&absolute, dir.metadata())?;
        // A fresh open has an independent directory cursor and remains anchored.
        let stream_fd = open_at(dir, OsStr::new("."), true, &absolute)?;
        use std::os::fd::IntoRawFd;
        let raw = stream_fd.into_raw_fd();
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            unsafe {
                libc::close(raw);
            }
            return Err(error(&absolute, ErrorKind::Io));
        }
        let stream = Directory(stream);
        loop {
            // readdir_r returns its error directly, avoiding platform errno APIs.
            let mut entry = std::mem::MaybeUninit::<libc::dirent>::uninit();
            let mut found = std::ptr::null_mut();
            let code = unsafe { libc::readdir_r(stream.0, entry.as_mut_ptr(), &mut found) };
            if code != 0 {
                return Err(error(&absolute, ErrorKind::Io));
            }
            if found.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*found).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if self.entries >= self.limits.max_entries {
                return Err(error(&absolute, ErrorKind::Limit));
            }
            self.entries += 1;
            let name = OsStr::from_bytes(name.to_bytes());
            let rel = relative.join(name);
            if self
                .selection
                .is_some_and(|selection| !selection.includes(&rel))
            {
                continue;
            }
            let path = self.root_path.join(&rel);
            let policy_path = self.policy_root.join(&rel);
            // Classify with fstatat before opening: never open devices/FIFOs.
            let c_name =
                CString::new(name.as_bytes()).map_err(|_| error(&path, ErrorKind::InvalidPath))?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    dir.as_raw_fd(),
                    c_name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(error(&path, ErrorKind::Io));
            }
            let stat = unsafe { stat.assume_init() };
            #[cfg(target_os = "linux")]
            let (stat_device, stat_mode) = (stat.st_dev, stat.st_mode);
            #[cfg(not(target_os = "linux"))]
            let (stat_device, stat_mode) = (stat.st_dev as u64, stat.st_mode as u32);
            if self
                .private_owner
                .is_some_and(|uid| stat.st_uid != uid || stat.st_mode & 0o7022 != 0)
            {
                return Err(error(&path, ErrorKind::InvalidPath));
            }
            let kind = stat.st_mode & libc::S_IFMT;
            let reason =
                if self.exclude_build_caches && kind == libc::S_IFDIR && is_build_cache(&rel) {
                    Some(ExclusionReason::BuildCache)
                } else if crate::path_policy::is_denied(&policy_path)
                    || crate::path_policy::is_denied(&path)
                {
                    Some(ExclusionReason::SecretName)
                } else if self
                    .secrets
                    .paths
                    .iter()
                    .any(|secret| path.starts_with(secret) || policy_path.starts_with(secret))
                {
                    Some(ExclusionReason::RegisteredSecret)
                } else if kind == libc::S_IFLNK {
                    Some(ExclusionReason::Symlink)
                } else if kind != libc::S_IFREG && kind != libc::S_IFDIR {
                    Some(ExclusionReason::SpecialFile)
                } else if kind == libc::S_IFREG && stat.st_nlink != 1 {
                    Some(ExclusionReason::MultipleLinks)
                } else if is_repository_metadata(&policy_path) || is_repository_metadata(&path) {
                    // Do not open even an ordinary .git file/directory: history and
                    // config may retain secrets excluded from the working tree.
                    Some(ExclusionReason::RepositoryMetadata)
                } else {
                    None
                };
            if let Some(reason) = reason {
                self.exclusions.push(Exclusion {
                    relative_path: rel,
                    reason,
                });
                continue;
            }
            let mut file = open_at(dir, name, kind == libc::S_IFDIR, &path)?;
            let metadata = io(&path, file.metadata())?;
            if metadata.dev() != stat_device
                || metadata.ino() != stat.st_ino
                || metadata.mode() != stat_mode
            {
                return Err(error(&path, ErrorKind::SourceChanged));
            }
            let reason = if self.secrets.denies(&path, &metadata)
                || self.secrets.denies(&policy_path, &metadata)
            {
                Some(ExclusionReason::RegisteredSecret)
            } else if metadata.is_file() && metadata.nlink() != 1 {
                Some(ExclusionReason::MultipleLinks)
            } else if !metadata.is_file() && !metadata.is_dir() {
                Some(ExclusionReason::SpecialFile)
            } else {
                None
            };
            if let Some(reason) = reason {
                self.exclusions.push(Exclusion {
                    relative_path: rel,
                    reason,
                });
                continue;
            }
            let destination = self.destination.map(|root| root.join(&rel));
            let mode = metadata.mode() & 0o777;
            if metadata.is_dir() {
                if depth >= self.limits.max_depth {
                    return Err(error(&path, ErrorKind::Limit));
                }
                if let Some(destination) = &destination {
                    io(destination, fs::create_dir(destination))?;
                }
                if self
                    .selection
                    .is_none_or(|selection| selection.descends(&rel))
                {
                    self.walk(&file, &rel, depth + 1)?;
                }
                // Keep the owned copy traversable/editable, including read-only input dirs.
                if let Some(destination) = &destination {
                    io(
                        destination,
                        fs::set_permissions(destination, Permissions::from_mode(mode | 0o700)),
                    )?;
                }
                self.manifest.push(ObservedTreeEntry {
                    relative_path: rel,
                    identity: Identity::of(&metadata),
                    bytes: 0,
                    mode,
                    sha256: None,
                    observed_mode: metadata.mode() & 0o7777,
                    observed_mtime: (metadata.mtime(), metadata.mtime_nsec()),
                    observed_ctime: (metadata.ctime(), metadata.ctime_nsec()),
                });
            } else {
                if self.files >= self.limits.max_files
                    || metadata.len() > self.limits.max_file_bytes
                    || metadata.len() > self.limits.max_total_bytes.saturating_sub(self.bytes)
                {
                    return Err(error(&path, ErrorKind::Limit));
                }
                self.files += 1;
                let mut target = destination
                    .as_ref()
                    .map(|destination| io(destination, File::create_new(destination)))
                    .transpose()?;
                let mut hash = Sha256::new();
                let mut count = 0u64;
                let mut buffer = [0u8; 16 * 1024];
                loop {
                    if self
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        return Err(error(&path, ErrorKind::Deadline));
                    }
                    let read = io(&path, file.read(&mut buffer))?;
                    if read == 0 {
                        break;
                    }
                    count += read as u64;
                    if count > metadata.len()
                        || count > self.limits.max_file_bytes
                        || count > self.limits.max_total_bytes.saturating_sub(self.bytes)
                    {
                        return Err(error(&path, ErrorKind::Limit));
                    }
                    hash.update(&buffer[..read]);
                    if let Some(target) = &mut target {
                        io(&path, target.write_all(&buffer[..read]))?;
                    }
                }
                if count != metadata.len() || !unchanged(&metadata, &io(&path, file.metadata())?) {
                    return Err(error(&path, ErrorKind::SourceChanged));
                }
                if let Some(target) = target {
                    io(&path, target.set_permissions(Permissions::from_mode(mode)))?;
                }
                self.bytes += count;
                self.manifest.push(ObservedTreeEntry {
                    relative_path: rel,
                    identity: Identity::of(&metadata),
                    bytes: count,
                    mode,
                    sha256: Some(hash.finalize().into()),
                    observed_mode: metadata.mode() & 0o7777,
                    observed_mtime: (metadata.mtime(), metadata.mtime_nsec()),
                    observed_ctime: (metadata.ctime(), metadata.ctime_nsec()),
                });
            }
        }
        if !unchanged(&before, &io(&absolute, dir.metadata())?) {
            return Err(error(&absolute, ErrorKind::SourceChanged));
        }
        Ok(())
    }
}

#[cfg(unix)]
pub fn is_repository_metadata(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .as_bytes()
            .eq_ignore_ascii_case(b".git")
    })
}

#[cfg(not(unix))]
pub fn is_repository_metadata(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
    })
}

pub fn is_build_cache(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("target" | "node_modules" | ".build" | ".cache" | "DerivedData")
        )
    })
}

#[derive(Default)]
pub struct Selection {
    pub exact: BTreeSet<PathBuf>,
    pub recursive: BTreeSet<PathBuf>,
}

impl Selection {
    pub fn includes(&self, path: &Path) -> bool {
        self.exact.contains(path) || self.recursive.iter().any(|root| path.starts_with(root))
    }

    #[cfg(unix)]
    fn descends(&self, path: &Path) -> bool {
        self.recursive.iter().any(|root| path.starts_with(root))
            || self
                .exact
                .iter()
                .any(|target| target != path && target.starts_with(path))
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_tree_admission_refuses_registry_before_consuming_paths() {
        let paths = std::iter::from_fn(|| -> Option<PathBuf> {
            panic!("unavailable registration must not consume paths")
        });
        assert_eq!(
            RegisteredSecrets::new(paths).unwrap_err().kind,
            ErrorKind::ProtectionUnavailable
        );
    }

    #[test]
    fn windows_tree_admission_refuses_mutation_and_preserves_metadata() {
        let path = PathBuf::from(r"C:\fixture\reserved");
        let identity = Identity {
            device: 7,
            inode: 9,
        };
        let mut secrets = RegisteredSecrets {
            paths: vec![path.clone()],
            identities: HashSet::from([identity]),
        };
        assert!(secrets.reserves(&path.join("child")));
        assert!(!secrets.reserves(Path::new(r"C:\unrelated")));
        assert_eq!(
            secrets.refreshed().unwrap_err().kind,
            ErrorKind::ProtectionUnavailable
        );
        assert_eq!(
            secrets.reserve_path(path.join("new")).unwrap_err().kind,
            ErrorKind::ProtectionUnavailable
        );
        assert_eq!(secrets.paths(), &[path]);
        assert_eq!(secrets.identities(), &HashSet::from([identity]));
    }

    #[test]
    fn windows_repository_metadata_keeps_component_boundaries() {
        assert!(is_repository_metadata(Path::new(r"C:\project\.GIT\config")));
        assert!(is_repository_metadata(Path::new(".git/config")));
        assert!(!is_repository_metadata(Path::new(
            r"C:\project\.github\config"
        )));
        assert!(!is_repository_metadata(Path::new("source/git.rs")));
    }
}
