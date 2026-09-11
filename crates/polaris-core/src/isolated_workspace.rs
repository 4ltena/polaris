//! Bounded, sanitized source snapshots. No apply-back or execution policy lives here.
//! Callers must serialize credential registration/rotation with snapshot creation.
//! This is not protection against a malicious, unsandboxed process of the same UID.

#![cfg(unix)]

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, Metadata, Permissions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use tempfile::TempDir;

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

impl Identity {
    fn of(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ManifestEntry {
    pub relative_path: PathBuf,
    pub identity: Identity,
    pub bytes: u64,
    /// Only ordinary rwx bits; setid and sticky bits are never preserved.
    pub mode: u32,
    /// None for directories.
    pub sha256: Option<[u8; 32]>,
    // Preserve special permission bits for source conflict detection only.
    // They must never be applied to the output workspace.
    observed_mode: u32,
}

impl ManifestEntry {
    /// Original source permission bits for approval/conflict evidence only.
    /// Use `mode` for installing output; special bits must not be restored.
    pub fn observed_mode(&self) -> u32 {
        self.observed_mode
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
type Result<T> = std::result::Result<T, SnapshotError>;
fn error(path: &Path, kind: ErrorKind) -> SnapshotError {
    SnapshotError {
        path: path.into(),
        kind,
    }
}
fn io<T>(path: &Path, value: std::io::Result<T>) -> Result<T> {
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

    pub(crate) fn denies(&self, path: &Path, m: &Metadata) -> bool {
        self.paths.iter().any(|secret| path.starts_with(secret))
            || self.identities.contains(&Identity::of(m))
    }
}

/// Owns a private temporary root containing workspace, home and tmp siblings.
/// Dropping this value removes all three, including scratch contents.
#[derive(Debug)]
pub struct Snapshot {
    _root: TempDir,
    workspace: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    copy_identity: Identity,
    source_ancestors: Vec<(PathBuf, Identity)>,
    pub source_path: PathBuf,
    pub source_identity: Identity,
    pub manifest: Vec<ManifestEntry>,
    pub exclusions: Vec<Exclusion>,
}
impl Snapshot {
    /// Sanitized workspace; only this directory participates in change scans.
    pub fn path(&self) -> &Path {
        &self.workspace
    }

    /// Private HOME sibling, excluded structurally from workspace change scans.
    pub fn scratch_home(&self) -> &Path {
        &self.home
    }

    /// Private temporary-file sibling, with the same lifetime as the workspace.
    pub fn scratch_tmp(&self) -> &Path {
        &self.tmp
    }
}

fn validate_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(error(path, ErrorKind::InvalidPath));
    }
    Ok(())
}

fn open_at(parent: &File, name: &OsStr, directory: bool, path: &Path) -> Result<File> {
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
fn open_root_with_ancestors(path: &Path) -> Result<(File, Vec<(PathBuf, Identity)>)> {
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

fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    Identity::of(a) == Identity::of(b)
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

/// The supplied source must be an absolute physical path, with no symlink
/// ancestors. Output is created under the OS temp directory, outside source.
/// Bounds or detected races fail the entire operation and discard partial output.
pub fn snapshot(source: &Path, secrets: &RegisteredSecrets, limits: Limits) -> Result<Snapshot> {
    snapshot_inner(source, secrets, limits, || {})
}

/// Authentication locations must be registered by their loader before this call.
/// Copying stays under the auth lock; credentials themselves never enter the copy.
pub fn protected_snapshot(source: &Path, limits: Limits) -> Result<Snapshot> {
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered)?;
        snapshot(source, &secrets, limits)
    })
    .map_err(|_| error(source, ErrorKind::ProtectionUnavailable))?
}

/// Capture candidate metadata under the authentication registry lock. Callers
/// must stop snapshot writers first and still revalidate at apply time. This
/// neither authorizes source writes nor keeps the lock after returning.
pub fn protected_collect_changes(snapshot: &Snapshot, limits: Limits) -> Result<ChangeSet> {
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered)?;
        collect_changes(snapshot, &secrets, limits)
    })
    .map_err(|_| error(&snapshot.source_path, ErrorKind::ProtectionUnavailable))?
}

fn snapshot_inner(
    source: &Path,
    secrets: &RegisteredSecrets,
    limits: Limits,
    after_open: impl FnOnce(),
) -> Result<Snapshot> {
    // Refresh identities for previously absent paths before any content read.
    // Retain old identities as well, covering a registered file moved aside.
    let refreshed = refreshed_secrets(secrets)?;
    let secrets = &refreshed;
    let (source_fd, source_ancestors) = open_root_with_ancestors(source)?;
    let initial = io(source, source_fd.metadata())?;
    if secrets.denies(source, &initial)
        || polaris_tools::path_policy::is_denied(source)
        || is_repository_metadata(source)
    {
        return Err(error(source, ErrorKind::InvalidPath));
    }
    let temp_parent = io(source, fs::canonicalize(std::env::temp_dir()))?;
    if temp_parent.starts_with(source) {
        return Err(error(source, ErrorKind::DestinationInsideSource));
    }
    let root = io(
        &temp_parent,
        tempfile::Builder::new()
            .prefix("polaris-workspace-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(&temp_parent),
    )?;
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let tmp = root.path().join("tmp");
    for directory in [&workspace, &home, &tmp] {
        io(
            directory,
            fs::DirBuilder::new().mode(0o700).create(directory),
        )?;
    }
    let copy_identity = Identity::of(&io(&workspace, fs::metadata(&workspace))?);
    let mut output = Snapshot {
        _root: root,
        workspace,
        home,
        tmp,
        copy_identity,
        source_ancestors: source_ancestors.clone(),
        source_path: source.into(),
        source_identity: Identity::of(&initial),
        manifest: Vec::new(),
        exclusions: Vec::new(),
    };
    after_open();
    let mut copier = Copier {
        secrets,
        limits,
        entries: 0,
        files: 0,
        bytes: 0,
        root_path: source,
        policy_root: source,
        destination: Some(output.path()),
        selection: None,
        manifest: Vec::new(),
        exclusions: Vec::new(),
    };
    copier.walk(&source_fd, Path::new(""), 0)?;
    let manifest = copier.manifest;
    let exclusions = copier.exclusions;
    output.manifest = manifest;
    output.exclusions = exclusions;
    let (current_fd, current_ancestors) = open_root_with_ancestors(source)?;
    let current = io(source, current_fd.metadata())?;
    if !unchanged(&initial, &current) || source_ancestors != current_ancestors {
        return Err(error(source, ErrorKind::SourceChanged));
    }
    Ok(output)
}

struct Copier<'a> {
    secrets: &'a RegisteredSecrets,
    limits: Limits,
    entries: usize,
    files: usize,
    bytes: u64,
    root_path: &'a Path,
    policy_root: &'a Path,
    destination: Option<&'a Path>,
    selection: Option<&'a Selection>,
    manifest: Vec<ManifestEntry>,
    exclusions: Vec<Exclusion>,
}

impl Copier<'_> {
    fn walk(&mut self, dir: &File, relative: &Path, depth: usize) -> Result<()> {
        let absolute = self.root_path.join(relative);
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
            let kind = stat.st_mode & libc::S_IFMT;
            let reason = if polaris_tools::path_policy::is_denied(&policy_path)
                || polaris_tools::path_policy::is_denied(&path)
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
            if metadata.dev() != stat.st_dev as u64
                || metadata.ino() != stat.st_ino
                || metadata.mode() != stat.st_mode as u32
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
                self.manifest.push(ManifestEntry {
                    relative_path: rel,
                    identity: Identity::of(&metadata),
                    bytes: 0,
                    mode,
                    sha256: None,
                    observed_mode: metadata.mode() & 0o7777,
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
                self.manifest.push(ManifestEntry {
                    relative_path: rel,
                    identity: Identity::of(&metadata),
                    bytes: count,
                    mode,
                    sha256: Some(hash.finalize().into()),
                    observed_mode: metadata.mode() & 0o7777,
                });
            }
        }
        if !unchanged(&before, &io(&absolute, dir.metadata())?) {
            return Err(error(&absolute, ErrorKind::SourceChanged));
        }
        Ok(())
    }
}

/// Semantic changes; replacing a file with a directory (or vice versa) is Modified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    ModeChanged,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub relative_path: PathBuf,
    pub kind: ChangeKind,
    /// Source baseline; absent for creations.
    pub before: Option<ManifestEntry>,
    /// Scanned copy metadata and hash, never file contents; absent for deletions.
    pub after: Option<ManifestEntry>,
}

#[derive(Debug)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// Ordinary .git exclusions are informational. Other exclusions, including
    /// ancestor changes that could remove .git, make the set ineligible.
    /// Excluded entries never become deletions; consult is_applicable().
    pub exclusions: Vec<Exclusion>,
    snapshot_path: PathBuf,
    copy_identity: Identity,
    source_identity: Identity,
}

impl ChangeSet {
    pub fn is_applicable(&self) -> bool {
        self.exclusions
            .iter()
            .all(is_informational_repository_exclusion)
    }
}

fn is_repository_metadata(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .as_bytes()
            .eq_ignore_ascii_case(b".git")
    })
}

fn is_informational_repository_exclusion(exclusion: &Exclusion) -> bool {
    exclusion.reason == ExclusionReason::RepositoryMetadata
        && is_repository_metadata(&exclusion.relative_path)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConflictReason {
    RootReplaced,
    AncestorReplaced,
    Missing,
    EntryChanged,
    CreationExists,
    UnexpectedEntry,
    Excluded,
    ChangedDuringScan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceConflict {
    /// Absolute source path; ancestors outside the workspace may be reported.
    pub path: PathBuf,
    pub reason: ConflictReason,
}

#[derive(Default)]
struct Selection {
    exact: BTreeSet<PathBuf>,
    recursive: BTreeSet<PathBuf>,
}

impl Selection {
    fn includes(&self, path: &Path) -> bool {
        self.exact.contains(path) || self.recursive.iter().any(|root| path.starts_with(root))
    }

    fn descends(&self, path: &Path) -> bool {
        self.recursive.iter().any(|root| path.starts_with(root))
            || self
                .exact
                .iter()
                .any(|target| target != path && target.starts_with(path))
    }
}

struct Scan {
    manifest: Vec<ManifestEntry>,
    exclusions: Vec<Exclusion>,
}

pub(crate) fn refreshed_secrets(secrets: &RegisteredSecrets) -> Result<RegisteredSecrets> {
    let mut refreshed = RegisteredSecrets::new(secrets.paths.clone())?;
    refreshed
        .identities
        .extend(secrets.identities.iter().copied());
    Ok(refreshed)
}

/// Shares the snapshot traversal with no destination, so this path cannot write
/// either scanned tree. Policy is checked at both physical and source locations.
fn scan(
    root: &Path,
    policy_root: &Path,
    expected_identity: Identity,
    secrets: &RegisteredSecrets,
    limits: Limits,
    selection: Option<&Selection>,
) -> Result<Scan> {
    let (fd, ancestors) = open_root_with_ancestors(root)?;
    let before = io(root, fd.metadata())?;
    if Identity::of(&before) != expected_identity {
        return Err(error(root, ErrorKind::SourceChanged));
    }
    if secrets.denies(root, &before)
        || secrets.denies(policy_root, &before)
        || polaris_tools::path_policy::is_denied(root)
        || polaris_tools::path_policy::is_denied(policy_root)
        || is_repository_metadata(root)
        || is_repository_metadata(policy_root)
    {
        return Err(error(root, ErrorKind::InvalidPath));
    }
    let mut walker = Copier {
        secrets,
        limits,
        entries: 0,
        files: 0,
        bytes: 0,
        root_path: root,
        policy_root,
        destination: None,
        selection,
        manifest: Vec::new(),
        exclusions: Vec::new(),
    };
    walker.walk(&fd, Path::new(""), 0)?;
    let (current, current_ancestors) = open_root_with_ancestors(root)?;
    if ancestors != current_ancestors || !unchanged(&before, &io(root, current.metadata())?) {
        return Err(error(root, ErrorKind::SourceChanged));
    }
    Ok(Scan {
        manifest: walker.manifest,
        exclusions: walker.exclusions,
    })
}

fn valid_relative(path: &Path, limits: Limits) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(error(path, ErrorKind::InvalidPath));
    }
    // max_depth counts directories; one final file component is permitted.
    if path.components().count().saturating_sub(1) > limits.max_depth {
        return Err(error(path, ErrorKind::Limit));
    }
    Ok(())
}

fn validate_baseline(snapshot: &Snapshot, limits: Limits) -> Result<()> {
    if snapshot
        .manifest
        .len()
        .saturating_add(snapshot.exclusions.len())
        > limits.max_entries
    {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in &snapshot.manifest {
        valid_relative(&entry.relative_path, limits)?;
        if entry.sha256.is_some() {
            files += 1;
            if files > limits.max_files
                || entry.bytes > limits.max_file_bytes
                || entry.bytes > limits.max_total_bytes.saturating_sub(bytes)
            {
                return Err(error(&entry.relative_path, ErrorKind::Limit));
            }
            bytes += entry.bytes;
        }
    }
    for excluded in &snapshot.exclusions {
        valid_relative(&excluded.relative_path, limits)?;
    }
    Ok(())
}

fn same_value(before: &ManifestEntry, after: &ManifestEntry) -> bool {
    before.sha256 == after.sha256 && before.bytes == after.bytes && before.mode == after.mode
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Call only after the child and its descendants have stopped. The result holds
/// hashes, not frozen contents; apply-back must revalidate any later content read.
/// No source or copy writes occur. No atomicity with external editors is claimed.
pub fn collect_changes(
    snapshot: &Snapshot,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<ChangeSet> {
    validate_baseline(snapshot, limits)?;
    let mut secrets = refreshed_secrets(secrets)?;
    // Baseline exclusions and newly protected source identities must prevent
    // hashing their copy counterparts, whose inodes differ from the source.
    for excluded in &snapshot.exclusions {
        // .git is already denied by traversal; do not upgrade this ordinary
        // informational exclusion into a blocking registered-secret exclusion.
        if excluded.reason != ExclusionReason::RepositoryMetadata {
            secrets
                .paths
                .push(snapshot.source_path.join(&excluded.relative_path));
        }
    }
    for entry in &snapshot.manifest {
        if secrets.identities.contains(&entry.identity) {
            secrets
                .paths
                .push(snapshot.source_path.join(&entry.relative_path));
        }
    }
    let current = scan(
        snapshot.path(),
        &snapshot.source_path,
        snapshot.copy_identity,
        &secrets,
        limits,
        None,
    )?;
    let before: BTreeMap<_, _> = snapshot
        .manifest
        .iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    let after: BTreeMap<_, _> = current
        .manifest
        .into_iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut boundaries: BTreeMap<PathBuf, ExclusionReason> = snapshot
        .exclusions
        .iter()
        .chain(current.exclusions.iter())
        .map(|entry| (entry.relative_path.clone(), entry.reason))
        .collect();
    // Newly registered secrets must also block deletion when absent from the copy.
    for path in &secrets.paths {
        for root in [&snapshot.source_path, snapshot.path()] {
            if let Ok(relative) = path.strip_prefix(root)
                && !relative.as_os_str().is_empty()
            {
                boundaries.insert(relative.into(), ExclusionReason::RegisteredSecret);
            }
        }
    }
    for entry in &snapshot.manifest {
        if secrets.identities.contains(&entry.identity) {
            boundaries.insert(
                entry.relative_path.clone(),
                ExclusionReason::RegisteredSecret,
            );
        }
    }
    let mut blocked: BTreeMap<_, _> = current
        .exclusions
        .into_iter()
        .map(|entry| (entry.relative_path, entry.reason))
        .collect();
    let mut changes = Vec::new();
    for path in paths {
        let old = before.get(&path).copied();
        let mut new = after.get(&path).cloned();
        if let (Some(old), Some(new)) = (old, new.as_mut()) {
            // Snapshot creation adds owner rwx to directories for working access.
            // That normalization alone is not a user-requested mode change.
            if old.sha256.is_none() && new.sha256.is_none() && new.mode == old.mode | 0o700 {
                new.mode = old.mode;
            }
        }
        let kind = match (old, new.as_ref()) {
            (None, Some(_)) => ChangeKind::Created,
            (Some(_), None) => ChangeKind::Deleted,
            (Some(old), Some(new)) if same_value(old, new) => continue,
            (Some(old), Some(new)) if old.sha256 == new.sha256 && old.bytes == new.bytes => {
                ChangeKind::ModeChanged
            }
            (Some(_), Some(_)) => ChangeKind::Modified,
            (None, None) => unreachable!(),
        };
        let mut excluded = false;
        for (boundary, reason) in &boundaries {
            if overlaps(&path, boundary) {
                // Removing/changing an ancestor must not indirectly remove Git
                // metadata. Report the ancestor as blocking, not the benign .git.
                let blocked_path = if *reason == ExclusionReason::RepositoryMetadata
                    && boundary.starts_with(&path)
                    && boundary != &path
                {
                    &path
                } else {
                    boundary
                };
                blocked.insert(blocked_path.clone(), *reason);
                excluded = true;
            }
        }
        if excluded {
            continue;
        }
        if changes.len() >= limits.max_entries {
            return Err(error(&path, ErrorKind::Limit));
        }
        changes.push(Change {
            relative_path: path,
            kind,
            before: old.cloned(),
            after: new,
        });
    }
    if changes.len().saturating_add(blocked.len()) > limits.max_entries {
        return Err(error(snapshot.path(), ErrorKind::Limit));
    }
    Ok(ChangeSet {
        changes,
        exclusions: blocked
            .into_iter()
            .map(|(relative_path, reason)| Exclusion {
                relative_path,
                reason,
            })
            .collect(),
        snapshot_path: snapshot.path().into(),
        copy_identity: snapshot.copy_identity,
        source_identity: snapshot.source_identity,
    })
}

fn source_roots_conflicts(snapshot: &Snapshot) -> Result<Vec<SourceConflict>> {
    let (root, ancestors) = match open_root_with_ancestors(&snapshot.source_path) {
        Ok(opened) => opened,
        Err(e) if e.kind == ErrorKind::Io => {
            return Ok(vec![SourceConflict {
                reason: if e.path == snapshot.source_path {
                    ConflictReason::RootReplaced
                } else {
                    ConflictReason::AncestorReplaced
                },
                path: e.path,
            }]);
        }
        Err(e) => return Err(e),
    };
    if Identity::of(&io(&snapshot.source_path, root.metadata())?) != snapshot.source_identity {
        return Ok(vec![SourceConflict {
            path: snapshot.source_path.clone(),
            reason: ConflictReason::RootReplaced,
        }]);
    }
    Ok(snapshot
        .source_ancestors
        .iter()
        .zip(ancestors.iter())
        .filter(|(before, after)| before != after)
        .map(|(before, _)| SourceConflict {
            path: before.0.clone(),
            reason: ConflictReason::AncestorReplaced,
        })
        .collect())
}

/// Advisory conflict check only. Hashes changed paths and inspects their
/// ancestors; directories being deleted/replaced are recursively checked for
/// unexpected children. Unrelated file contents are not read. Even an empty
/// result is not an atomic compare-and-swap guarantee for a later apply-back.
pub fn check_source_conflicts(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<Vec<SourceConflict>> {
    validate_baseline(snapshot, limits)?;
    if changes.snapshot_path != snapshot.path()
        || changes.copy_identity != snapshot.copy_identity
        || changes.source_identity != snapshot.source_identity
    {
        return Err(error(&snapshot.source_path, ErrorKind::InvalidPath));
    }
    if changes
        .changes
        .len()
        .saturating_add(changes.exclusions.len())
        > limits.max_entries
    {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    let roots = source_roots_conflicts(snapshot)?;
    if !roots.is_empty() {
        return Ok(roots);
    }
    let mut selection = Selection::default();
    let baseline: BTreeMap<_, _> = snapshot
        .manifest
        .iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    let mut conflicts = BTreeSet::new();
    for exclusion in &changes.exclusions {
        valid_relative(&exclusion.relative_path, limits)?;
        if is_informational_repository_exclusion(exclusion) {
            continue;
        }
        conflicts.insert((
            snapshot.source_path.join(&exclusion.relative_path),
            ConflictReason::Excluded,
        ));
    }
    for change in &changes.changes {
        valid_relative(&change.relative_path, limits)?;
        for ancestor in change
            .relative_path
            .ancestors()
            .filter(|path| !path.as_os_str().is_empty())
        {
            selection.exact.insert(ancestor.into());
        }
        if baseline
            .get(&change.relative_path)
            .is_some_and(|entry| entry.sha256.is_none())
            && change
                .after
                .as_ref()
                .is_none_or(|entry| entry.sha256.is_some())
        {
            selection.recursive.insert(change.relative_path.clone());
        }
    }
    if selection.exact.len() > limits.max_entries {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    if !selection.exact.is_empty() {
        let secrets = refreshed_secrets(secrets)?;
        let current = match scan(
            &snapshot.source_path,
            &snapshot.source_path,
            snapshot.source_identity,
            &secrets,
            limits,
            Some(&selection),
        ) {
            Ok(current) => current,
            Err(e) if e.kind == ErrorKind::SourceChanged => {
                return Ok(vec![SourceConflict {
                    path: e.path,
                    reason: ConflictReason::ChangedDuringScan,
                }]);
            }
            Err(e) => return Err(e),
        };
        let present: BTreeMap<_, _> = current
            .manifest
            .iter()
            .map(|entry| (entry.relative_path.clone(), entry))
            .collect();
        let paths: BTreeSet<_> = selection
            .exact
            .iter()
            .cloned()
            .chain(
                baseline
                    .keys()
                    .filter(|path| selection.includes(path))
                    .cloned(),
            )
            .chain(present.keys().cloned())
            .collect();
        for path in paths {
            if current
                .exclusions
                .iter()
                .any(|excluded| path.starts_with(&excluded.relative_path))
            {
                conflicts.insert((snapshot.source_path.join(&path), ConflictReason::Excluded));
                continue;
            }
            let reason = match (baseline.get(&path), present.get(&path)) {
                (Some(old), Some(now))
                    if old.identity == now.identity
                        && old.observed_mode == now.observed_mode
                        && same_value(old, now) =>
                {
                    None
                }
                (Some(old), Some(now))
                    if old.sha256.is_none()
                        && (old.identity != now.identity || now.sha256.is_some()) =>
                {
                    Some(ConflictReason::AncestorReplaced)
                }
                (Some(_), Some(_)) => Some(ConflictReason::EntryChanged),
                (Some(_), None) => Some(ConflictReason::Missing),
                (None, Some(_)) if selection.exact.contains(&path) => {
                    Some(ConflictReason::CreationExists)
                }
                (None, Some(_)) => Some(ConflictReason::UnexpectedEntry),
                (None, None) => None,
            };
            if let Some(reason) = reason {
                conflicts.insert((snapshot.source_path.join(path), reason));
            }
        }
        for excluded in current.exclusions {
            conflicts.insert((
                snapshot.source_path.join(excluded.relative_path),
                ConflictReason::Excluded,
            ));
        }
    }
    let roots = source_roots_conflicts(snapshot)?;
    conflicts.extend(
        roots
            .into_iter()
            .map(|conflict| (conflict.path, conflict.reason)),
    );
    if conflicts.len() > limits.max_entries {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    Ok(conflicts
        .into_iter()
        .map(|(path, reason)| SourceConflict { path, reason })
        .collect())
}

#[cfg(test)]
mod tests;
