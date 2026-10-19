//! C10's private, descriptor-relative local Git repository owner.
//!
//! Inspection never initializes, repairs, refreshes an index or stages files.
//! Mutation requires the same project EX and actual durable C10 effect/intent.
//! The project itself is only read through pinned no-follow descriptors.

#[cfg(unix)]
use super::{MAX_FILE_BYTES, MAX_FILES, MAX_TOTAL_BYTES};
use crate::{
    management_effect::{
        ManagementOperationIdV1, StartedManagementEffectV1, UnknownManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
    sync::{
        push::C10ExactRecoveryFetchV1,
        remote_intent::{
            SyncDigestV1, SyncGitObjectIdV1, SyncPayloadDescriptorV1, SyncPayloadEntryV1,
            SyncPriorHeadV2,
        },
        share::{ShareManifest, SyncPaths},
    },
};
#[cfg(unix)]
use crate::{
    project_mutation::open_directory_nofollow,
    sync::{
        owner::C10IntentStoreV2,
        remote_intent::{SyncLocalPreparedV2, SyncPayloadModeV1, SyncRemoteIntentStateV2},
    },
};
use polaris_auth::git_credentials::{
    CanonicalHttpsRemoteV1, ExecutableIdentityV1, GitChildOutputV1, GitCredentialBindingV1,
    GitCredentialMaterialV1, GitCredentialOwnerV1,
};
#[cfg(unix)]
use polaris_auth::git_credentials::{
    GitChildFailurePhaseV1, git_blob_oid_from_bytes, run_bound_local_git,
};
#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, OsStr},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Component,
};

#[cfg(unix)]
const CONFIG_BYTES: u64 = 64 * 1024;
#[cfg(unix)]
const CONTROL_BYTES: u64 = 1024 * 1024;
#[cfg(unix)]
const ARG_BATCH_BYTES: usize = 24 * 1024;
#[cfg(unix)]
const OUTPUT_BATCH_BYTES: usize = 48 * 1024;
#[cfg(unix)]
const REQUIRED_FSYNC: &str =
    "core.fsync=loose-object,pack,pack-metadata,commit-graph,index,reference";
#[cfg(unix)]
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, thiserror::Error)]
pub(crate) enum C10RepositoryErrorV1 {
    #[error("c10_repository_protection")]
    Protection,
    #[error("c10_repository_changed")]
    Changed,
    #[error("c10_repository_dirty")]
    Dirty,
    #[error("c10_repository_corrupt")]
    Corrupt,
    #[error("c10_repository_bounds")]
    Bounds,
    #[error("c10_repository_unavailable")]
    Unavailable,
    #[error("c10_local_git_before_spawn")]
    BeforeSpawn,
    #[error("c10_local_git_after_spawn")]
    AfterSpawn,
    #[error("c10_local_git_failed")]
    GitFailed,
    #[error("c10_repository_storage")]
    Io(#[from] io::Error),
}
type Result<T> = std::result::Result<T, C10RepositoryErrorV1>;

#[derive(Clone, Copy, Eq, PartialEq)]
struct Anchor {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
}
#[cfg(unix)]
impl Anchor {
    fn of(m: &fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            uid: m.uid(),
            mode: m.mode(),
        }
    }
    fn stat(m: &libc::stat) -> Self {
        Self {
            dev: m.st_dev as u64,
            #[cfg(target_pointer_width = "64")]
            ino: m.st_ino,
            #[cfg(not(target_pointer_width = "64"))]
            ino: m.st_ino as u64,
            uid: m.st_uid,
            mode: m.st_mode as u32,
        }
    }
}
#[cfg(unix)]
fn same_metadata(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    Anchor::of(a) == Anchor::of(b)
        && (
            a.nlink(),
            a.len(),
            a.mtime(),
            a.mtime_nsec(),
            a.ctime(),
            a.ctime_nsec(),
        ) == (
            b.nlink(),
            b.len(),
            b.mtime(),
            b.mtime_nsec(),
            b.ctime(),
            b.ctime_nsec(),
        )
}
#[cfg(unix)]
fn owned_directory(m: &fs::Metadata, private: bool) -> Result<()> {
    if !m.is_dir()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o022 != 0
        || private && m.mode() & 0o7777 != 0o700
    {
        return Err(C10RepositoryErrorV1::Protection);
    }
    Ok(())
}
#[cfg(unix)]
fn owned_file(m: &fs::Metadata, private: bool) -> Result<()> {
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.nlink() != 1
        || m.mode() & 0o022 != 0
        || m.mode() & 0o7000 != 0
        || private && m.mode() & 0o7777 != 0o600
    {
        return Err(C10RepositoryErrorV1::Protection);
    }
    Ok(())
}
#[cfg(unix)]
fn name(value: &OsStr) -> Result<CString> {
    if value.as_bytes().is_empty()
        || value.as_bytes() == b"."
        || value.as_bytes() == b".."
        || value.as_bytes().contains(&b'/')
    {
        return Err(C10RepositoryErrorV1::Protection);
    }
    CString::new(value.as_bytes()).map_err(|_| C10RepositoryErrorV1::Protection)
}
#[cfg(unix)]
fn stat_at(parent: &File, component: &OsStr) -> Result<Option<libc::stat>> {
    let component = name(component)?;
    let mut output = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            component.as_ptr(),
            output.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    Ok(Some(unsafe { output.assume_init() }))
}
#[cfg(unix)]
fn open_at(parent: &File, component: &OsStr, flags: i32, mode: u32) -> Result<File> {
    let component = name(component)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[cfg(unix)]
fn open_directory_at(parent: &File, component: &OsStr) -> Result<Option<File>> {
    let Some(before) = stat_at(parent, component)? else {
        return Ok(None);
    };
    if before.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let file = open_at(parent, component, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let metadata = file.metadata()?;
    owned_directory(&metadata, false)?;
    if Anchor::stat(&before) != Anchor::of(&metadata) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    verify_named(parent, component, Anchor::of(&metadata))?;
    Ok(Some(file))
}
#[cfg(unix)]
fn verify_named(parent: &File, component: &OsStr, expected: Anchor) -> Result<()> {
    if stat_at(parent, component)?.map(|m| Anchor::stat(&m)) != Some(expected) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}

struct Ancestor {
    parent: File,
    component: OsString,
    anchor: Anchor,
}
struct DirectoryPath {
    path: PathBuf,
    directory: File,
    ancestors: Vec<Ancestor>,
}
#[cfg(unix)]
impl DirectoryPath {
    fn pin(path: &Path, private: bool) -> Result<Self> {
        if !path.is_absolute() || fs::canonicalize(path)? != path {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let mut directory = open_directory_nofollow(Path::new("/"))?;
        let mut ancestors = Vec::new();
        for component in path.components() {
            let component = match component {
                Component::RootDir => continue,
                Component::Normal(value) => value,
                _ => return Err(C10RepositoryErrorV1::Protection),
            };
            let before = stat_at(&directory, component)?.ok_or(C10RepositoryErrorV1::Changed)?;
            if before.st_mode & libc::S_IFMT != libc::S_IFDIR {
                return Err(C10RepositoryErrorV1::Protection);
            }
            let next = open_at(&directory, component, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let metadata = next.metadata()?;
            let euid = unsafe { libc::geteuid() };
            // A root-owned sticky temporary ancestor cannot rename this user's
            // owned child. Every actual control directory remains owner-only.
            let sticky_root = metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX as u32 != 0;
            if metadata.uid() != 0 && metadata.uid() != euid
                || metadata.mode() & 0o022 != 0 && !sticky_root
                || Anchor::stat(&before) != Anchor::of(&metadata)
            {
                return Err(C10RepositoryErrorV1::Protection);
            }
            ancestors.push(Ancestor {
                parent: directory,
                component: component.to_owned(),
                anchor: Anchor::of(&metadata),
            });
            directory = next;
        }
        owned_directory(&directory.metadata()?, private)?;
        let result = Self {
            path: path.to_owned(),
            directory,
            ancestors,
        };
        result.verify()?;
        Ok(result)
    }
    fn verify(&self) -> Result<()> {
        for ancestor in &self.ancestors {
            verify_named(&ancestor.parent, &ancestor.component, ancestor.anchor)?;
        }
        if fs::canonicalize(&self.path)? != self.path {
            return Err(C10RepositoryErrorV1::Changed);
        }
        Ok(())
    }
}

#[cfg(unix)]
struct FileRead {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
}
#[cfg(unix)]
fn read_file(parent: &File, component: &OsStr, limit: u64, private: bool) -> Result<FileRead> {
    let before = stat_at(parent, component)?.ok_or(C10RepositoryErrorV1::Changed)?;
    if before.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let mut file = open_at(parent, component, libc::O_RDONLY, 0)?;
    let metadata = file.metadata()?;
    owned_file(&metadata, private)?;
    if metadata.len() > limit || Anchor::of(&metadata) != Anchor::stat(&before) {
        return Err(C10RepositoryErrorV1::Bounds);
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit || !same_metadata(&metadata, &file.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    verify_named(parent, component, Anchor::of(&metadata))?;
    // Reopen after reading: a same-inode write is also a changed snapshot.
    let reopened = open_at(parent, component, libc::O_RDONLY, 0)?;
    if !same_metadata(&metadata, &reopened.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(FileRead { bytes, metadata })
}

/// Readdir is streaming: empty directories do not consume payload file slots
/// and do not require an invented lower compatibility bound.
#[cfg(unix)]
struct DirectoryStream {
    stream: *mut libc::DIR,
    #[cfg(test)]
    identity: (u64, u64),
}
#[cfg(unix)]
impl DirectoryStream {
    fn new(directory: &File) -> Result<Self> {
        // "." is deliberately not accepted by the general component helper.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error.into());
        }
        #[cfg(test)]
        let metadata = directory.metadata()?;
        Ok(Self {
            stream,
            #[cfg(test)]
            identity: (metadata.dev(), metadata.ino()),
        })
    }
    fn next(&mut self) -> Result<Option<OsString>> {
        use std::os::unix::ffi::OsStringExt;
        loop {
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(self.stream) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error().unwrap_or(0) != 0 {
                    return Err(error.into());
                }
                #[cfg(test)]
                {
                    let callback = AFTER_EOF.with(|slot| {
                        let mut slot = slot.borrow_mut();
                        if slot
                            .as_ref()
                            .is_some_and(|(identity, _)| *identity == self.identity)
                        {
                            slot.take().map(|(_, callback)| callback)
                        } else {
                            None
                        }
                    });
                    if let Some(callback) = callback {
                        callback();
                    }
                }
                return Ok(None);
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            return Ok(Some(OsString::from_vec(bytes.to_vec())));
        }
    }
}
#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.stream);
        }
    }
}

struct PayloadFile {
    entry: SyncPayloadEntryV1,
    git_oid: String,
    metadata: fs::Metadata,
}
pub(crate) struct C10PayloadSnapshotV1 {
    project: DirectoryPath,
    manifest: ShareManifest,
    files: Vec<PayloadFile>,
    descriptor: SyncPayloadDescriptorV1,
    excluded: usize,
}
impl C10PayloadSnapshotV1 {
    #[cfg(unix)]
    pub(crate) fn capture(project: &Path, manifest: &ShareManifest) -> Result<Self> {
        let project = DirectoryPath::pin(project, false)?;
        let (files, excluded) = scan_payload(&project.directory, Some(manifest))?;
        let descriptor = descriptor(&files)?;
        let result = Self {
            project,
            manifest: manifest.clone(),
            files,
            descriptor,
            excluded,
        };
        result.revalidate()?;
        Ok(result)
    }
    pub(crate) fn descriptor(&self) -> &SyncPayloadDescriptorV1 {
        &self.descriptor
    }
    pub(crate) fn excluded_entries(&self) -> usize {
        self.excluded
    }
    pub(crate) fn manifest_digest(&self) -> String {
        self.manifest.digest()
    }
    #[cfg(unix)]
    pub(crate) fn revalidate(&self) -> Result<()> {
        self.project.verify()?;
        let (files, excluded) = scan_payload(&self.project.directory, Some(&self.manifest))?;
        if excluded != self.excluded
            || descriptor(&files)? != self.descriptor
            || files.iter().zip(&self.files).any(|(a, b)| {
                a.entry != b.entry
                    || a.git_oid != b.git_oid
                    || !same_metadata(&a.metadata, &b.metadata)
            })
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.project.verify()
    }
}
#[cfg(unix)]
fn descriptor(files: &[PayloadFile]) -> Result<SyncPayloadDescriptorV1> {
    SyncPayloadDescriptorV1::new(files.iter().map(|file| file.entry.clone()).collect())
        .map_err(|_| C10RepositoryErrorV1::Bounds)
}
#[cfg(unix)]
fn scan_payload(
    root: &File,
    manifest: Option<&ShareManifest>,
) -> Result<(Vec<PayloadFile>, usize)> {
    let mut files = Vec::new();
    let mut bytes = 0_u64;
    let mut excluded = 0;
    walk_payload(
        root,
        Path::new(""),
        manifest,
        &mut files,
        &mut bytes,
        &mut excluded,
    )?;
    files.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    let _ = descriptor(&files)?;
    Ok((files, excluded))
}
#[cfg(unix)]
fn walk_payload(
    directory: &File,
    relative: &Path,
    manifest: Option<&ShareManifest>,
    files: &mut Vec<PayloadFile>,
    bytes: &mut u64,
    excluded: &mut usize,
) -> Result<()> {
    let before = directory.metadata()?;
    owned_directory(&before, false)?;
    let mut stream = DirectoryStream::new(directory)?;
    while let Some(component) = stream.next()? {
        let child = relative.join(&component);
        // These are the same native path limits used by the original staging
        // filesystem API, not a new file or empty-directory count limit.
        if child.as_os_str().as_bytes().len() >= libc::PATH_MAX as usize {
            return Err(C10RepositoryErrorV1::Bounds);
        }
        let Some(path) = child.to_str() else {
            return Err(C10RepositoryErrorV1::Protection);
        };
        if manifest.is_some_and(|manifest| !manifest.shares(&child)) {
            *excluded += 1;
            continue;
        }
        let entry = stat_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
        match entry.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child_directory = open_directory_at(directory, &component)?
                    .ok_or(C10RepositoryErrorV1::Changed)?;
                walk_payload(&child_directory, &child, manifest, files, bytes, excluded)?;
                verify_named(
                    directory,
                    &component,
                    Anchor::of(&child_directory.metadata()?),
                )?;
            }
            libc::S_IFREG => {
                if files.len() >= MAX_FILES {
                    return Err(C10RepositoryErrorV1::Bounds);
                }
                let file = read_file(directory, &component, MAX_FILE_BYTES, false)?;
                *bytes = bytes
                    .checked_add(file.bytes.len() as u64)
                    .ok_or(C10RepositoryErrorV1::Bounds)?;
                if *bytes > MAX_TOTAL_BYTES {
                    return Err(C10RepositoryErrorV1::Bounds);
                }
                files.push(PayloadFile {
                    entry: SyncPayloadEntryV1 {
                        path: path.to_owned(),
                        mode: if file.metadata.mode() & 0o111 != 0 {
                            SyncPayloadModeV1::Executable
                        } else {
                            SyncPayloadModeV1::Regular
                        },
                        bytes: file.bytes.len() as u64,
                        content_hash: SyncDigestV1::digest(&file.bytes),
                    },
                    git_oid: git_blob_oid_from_bytes(&file.bytes),
                    metadata: file.metadata,
                });
            }
            _ => return Err(C10RepositoryErrorV1::Protection),
        }
    }
    if !same_metadata(&before, &directory.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}

struct ConfigPin {
    metadata: fs::Metadata,
    hash: SyncDigestV1,
    values: BTreeMap<String, String>,
}
#[cfg(any(unix, test))]
fn parse_config(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes).map_err(|_| C10RepositoryErrorV1::Corrupt)?;
    let mut section = false;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            if section || !line.eq_ignore_ascii_case("[core]") {
                return Err(C10RepositoryErrorV1::Protection);
            }
            section = true;
            continue;
        }
        if !section {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let (key, value) = line
            .split_once('=')
            .ok_or(C10RepositoryErrorV1::Protection)?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        let valid = match key.as_str() {
            "repositoryformatversion" => value == "0",
            "bare" => value == "false",
            "logallrefupdates" => value == "true",
            "filemode" | "ignorecase" | "precomposeunicode" => {
                matches!(value.as_str(), "true" | "false")
            }
            _ => false,
        };
        if !valid || values.insert(key, value).is_some() {
            return Err(C10RepositoryErrorV1::Protection);
        }
    }
    for required in [
        "repositoryformatversion",
        "filemode",
        "bare",
        "logallrefupdates",
    ] {
        if !values.contains_key(required) {
            return Err(C10RepositoryErrorV1::Protection);
        }
    }
    Ok(values)
}
#[cfg(unix)]
fn pin_config(repository: &File) -> Result<ConfigPin> {
    let read = read_file(repository, OsStr::new("config"), CONFIG_BYTES, false)?;
    Ok(ConfigPin {
        hash: SyncDigestV1::digest(&read.bytes),
        values: parse_config(&read.bytes)?,
        metadata: read.metadata,
    })
}
#[cfg(unix)]
impl ConfigPin {
    fn verify(&self, repository: &File) -> Result<()> {
        let read = read_file(repository, OsStr::new("config"), CONFIG_BYTES, false)?;
        if !same_metadata(&self.metadata, &read.metadata)
            || self.hash != SyncDigestV1::digest(&read.bytes)
            || self.values != parse_config(&read.bytes)?
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct C10RepositorySnapshotV1 {
    head: SyncPriorHeadV2,
    descriptor: SyncPayloadDescriptorV1,
    index_digest: SyncDigestV1,
    staging_digest: SyncDigestV1,
}
impl C10RepositorySnapshotV1 {
    pub(crate) fn head(&self) -> &SyncPriorHeadV2 {
        &self.head
    }
    pub(crate) fn descriptor(&self) -> &SyncPayloadDescriptorV1 {
        &self.descriptor
    }
    pub(crate) fn index_digest(&self) -> SyncDigestV1 {
        self.index_digest
    }
    pub(crate) fn staging_digest(&self) -> SyncDigestV1 {
        self.staging_digest
    }
}
pub(crate) enum C10RepositoryInspectionV1 {
    Absent(C10AbsentRepositoryV1),
    Existing(Box<C10LocalRepositoryV1>),
}
pub(crate) struct C10AbsentRepositoryV1 {
    state: DirectoryPath,
    git: ExecutableIdentityV1,
}
pub(crate) struct C10LocalRepositoryV1 {
    state: DirectoryPath,
    sync: DirectoryPath,
    repository: DirectoryPath,
    staging: DirectoryPath,
    paths: SyncPaths,
    git: ExecutableIdentityV1,
    config: ConfigPin,
}

/// Non-Clone/non-Serde evidence from actual fsync and create-free reopen.
pub(crate) struct C10CommitReadbackV1 {
    operation: ManagementOperationIdV1,
    request_hash: SyncDigestV1,
    commit: SyncGitObjectIdV1,
    prior_head: SyncPriorHeadV2,
    descriptor: SyncPayloadDescriptorV1,
    objects_digest: SyncDigestV1,
    created: bool,
}
impl C10CommitReadbackV1 {
    pub(crate) fn operation_id(&self) -> ManagementOperationIdV1 {
        self.operation
    }
    pub(crate) fn request_hash(&self) -> SyncDigestV1 {
        self.request_hash
    }
    pub(crate) fn commit(&self) -> &SyncGitObjectIdV1 {
        &self.commit
    }
    pub(crate) fn prior_head(&self) -> &SyncPriorHeadV2 {
        &self.prior_head
    }
    pub(crate) fn descriptor(&self) -> &SyncPayloadDescriptorV1 {
        &self.descriptor
    }
    pub(crate) fn objects_digest(&self) -> SyncDigestV1 {
        self.objects_digest
    }
    pub(crate) fn created(&self) -> bool {
        self.created
    }
}
/// Restoration cannot be minted from a caller's success boolean.
pub(crate) struct C10RestoreReadbackV1 {
    operation: ManagementOperationIdV1,
    request_hash: SyncDigestV1,
    expected_current: SyncPriorHeadV2,
    restored: SyncPriorHeadV2,
    descriptor: SyncPayloadDescriptorV1,
    index_digest: SyncDigestV1,
    staging_digest: SyncDigestV1,
    objects_digest: SyncDigestV1,
}
impl C10RestoreReadbackV1 {
    pub(crate) fn operation_id(&self) -> ManagementOperationIdV1 {
        self.operation
    }
    pub(crate) fn request_hash(&self) -> SyncDigestV1 {
        self.request_hash
    }
    pub(crate) fn expected_current(&self) -> &SyncPriorHeadV2 {
        &self.expected_current
    }
    pub(crate) fn restored_head(&self) -> &SyncPriorHeadV2 {
        &self.restored
    }
    pub(crate) fn descriptor(&self) -> &SyncPayloadDescriptorV1 {
        &self.descriptor
    }
    pub(crate) fn index_digest(&self) -> SyncDigestV1 {
        self.index_digest
    }
    pub(crate) fn staging_digest(&self) -> SyncDigestV1 {
        self.staging_digest
    }
    pub(crate) fn objects_digest(&self) -> SyncDigestV1 {
        self.objects_digest
    }
}

#[cfg(any(unix, test))]
fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
#[cfg(any(unix, test))]
fn safe_ref_component(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.ends_with(".lock")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
#[cfg(any(unix, test))]
fn control_kind(relative: &str, name: &str, is_directory: bool) -> Result<()> {
    let allowed = match relative {
        "" => {
            if is_directory {
                matches!(name, "objects" | "refs" | "logs" | "hooks" | "info")
            } else {
                matches!(
                    name,
                    "HEAD"
                        | "config"
                        | "description"
                        | "packed-refs"
                        | "ORIG_HEAD"
                        | "FETCH_HEAD"
                        | "COMMIT_EDITMSG"
                )
            }
        }
        "objects" => is_directory && (matches!(name, "info" | "pack") || valid_hex(name, 2)),
        "objects/info" => {
            if is_directory {
                name == "commit-graphs"
            } else {
                matches!(name, "packs" | "commit-graph")
            }
        }
        "objects/info/commit-graphs" => {
            !is_directory
                && (name == "commit-graph-chain"
                    || name
                        .strip_prefix("graph-")
                        .and_then(|n| n.strip_suffix(".graph"))
                        .is_some_and(|n| valid_hex(n, 40)))
        }
        "objects/pack" => {
            !is_directory
                && name
                    .strip_prefix("pack-")
                    .and_then(|n| n.split_once('.'))
                    .is_some_and(|(hash, suffix)| {
                        valid_hex(hash, 40)
                            && matches!(
                                suffix,
                                "pack" | "idx" | "rev" | "bitmap" | "keep" | "mtimes"
                            )
                    })
        }
        "hooks" => !is_directory && name.ends_with(".sample") && safe_ref_component(name),
        "info" => !is_directory && name == "exclude",
        "refs" => is_directory && matches!(name, "heads" | "tags" | "polaris"),
        "logs" => {
            if is_directory {
                name == "refs"
            } else {
                name == "HEAD"
            }
        }
        "logs/refs" => is_directory && matches!(name, "heads" | "tags" | "polaris"),
        "refs/heads" | "logs/refs/heads" => !is_directory && matches!(name, "polaris" | "incoming"),
        "refs/tags" | "logs/refs/tags" => false,
        "refs/polaris" | "logs/refs/polaris" => is_directory && name == "recovery",
        "refs/polaris/recovery" | "logs/refs/polaris/recovery" => {
            !is_directory && valid_hex(name, 64)
        }
        "logs/HEAD" => false,
        _ if relative.starts_with("objects/") && relative.len() == 10 => {
            !is_directory && valid_hex(name, 38)
        }
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(C10RepositoryErrorV1::Protection)
    }
}
#[cfg(unix)]
fn hash_file(parent: &File, component: &OsStr, fsync: bool) -> Result<(fs::Metadata, [u8; 32])> {
    let stat = stat_at(parent, component)?.ok_or(C10RepositoryErrorV1::Changed)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let mut file = open_at(parent, component, libc::O_RDONLY, 0)?;
    let before = file.metadata()?;
    owned_file(&before, false)?;
    if Anchor::of(&before) != Anchor::stat(&stat) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if fsync {
        file.sync_all()?;
        fault(FaultPoint::FileFsync)?;
    }
    if !same_metadata(&before, &file.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    verify_named(parent, component, Anchor::of(&before))?;
    Ok((before, hasher.finalize().into()))
}
/// A streaming multiset commitment has no inventory-sized allocation. Path is
/// part of every entry hash; a directory cannot contain the same path twice.
/// The semantic commit/tree proof below is independent of this fsync receipt.
#[cfg(unix)]
struct ObjectProjection {
    count: u64,
    xor: [u8; 32],
    sum: [u8; 32],
}
#[cfg(unix)]
impl ObjectProjection {
    fn new() -> Self {
        Self {
            count: 0,
            xor: [0; 32],
            sum: [0; 32],
        }
    }
    fn add(
        &mut self,
        path: &str,
        metadata: &fs::Metadata,
        content: Option<[u8; 32]>,
    ) -> Result<()> {
        let mut hash = Sha256::new();
        hash.update(b"polaris-c10-local-object-entry-v1\0");
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path.as_bytes());
        for value in [
            metadata.dev(),
            metadata.ino(),
            metadata.uid() as u64,
            metadata.mode() as u64,
            metadata.nlink(),
            metadata.len(),
        ] {
            hash.update(value.to_be_bytes());
        }
        if let Some(content) = content {
            hash.update(content);
        }
        let digest: [u8; 32] = hash.finalize().into();
        let mut carry = 0_u16;
        for index in (0..32).rev() {
            self.xor[index] ^= digest[index];
            let value = self.sum[index] as u16 + digest[index] as u16 + carry;
            self.sum[index] = value as u8;
            carry = value >> 8;
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(C10RepositoryErrorV1::Bounds)?;
        Ok(())
    }
    fn finish(self) -> SyncDigestV1 {
        let mut bytes = b"polaris-c10-local-durable-object-inventory-v1\0".to_vec();
        bytes.extend_from_slice(&self.count.to_be_bytes());
        bytes.extend_from_slice(&self.xor);
        bytes.extend_from_slice(&self.sum);
        SyncDigestV1::digest(&bytes)
    }
}
#[cfg(unix)]
fn scan_controls(
    directory: &File,
    relative: &str,
    fsync: bool,
    projection: &mut ObjectProjection,
) -> Result<()> {
    let before = directory.metadata()?;
    owned_directory(&before, false)?;
    let mut stream = DirectoryStream::new(directory)?;
    while let Some(component) = stream.next()? {
        let child_name = component.to_str().ok_or(C10RepositoryErrorV1::Protection)?;
        let entry = stat_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
        let directory_kind = entry.st_mode & libc::S_IFMT == libc::S_IFDIR;
        control_kind(relative, child_name, directory_kind)?;
        let path = if relative.is_empty() {
            child_name.to_owned()
        } else {
            format!("{relative}/{child_name}")
        };
        if directory_kind {
            let child =
                open_directory_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
            scan_controls(&child, &path, fsync, projection)?;
            verify_named(directory, &component, Anchor::of(&child.metadata()?))?;
        } else {
            let (metadata, hash) = hash_file(directory, &component, fsync)?;
            if relative.starts_with("refs/") {
                let read = read_file(directory, &component, CONTROL_BYTES, false)?;
                parse_oid_line(&read.bytes)?;
            }
            projection.add(&path, &metadata, Some(hash))?;
        }
    }
    if fsync {
        directory.sync_all()?;
        fault(FaultPoint::DirectoryFsync)?;
    }
    if !same_metadata(&before, &directory.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    projection.add(relative, &before, None)
}
#[cfg(unix)]
fn validate_controls(directory: &File, relative: &str) -> Result<()> {
    let before = directory.metadata()?;
    owned_directory(&before, false)?;
    let mut stream = DirectoryStream::new(directory)?;
    while let Some(component) = stream.next()? {
        let component_text = component.to_str().ok_or(C10RepositoryErrorV1::Protection)?;
        let before_entry = stat_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
        let is_directory = before_entry.st_mode & libc::S_IFMT == libc::S_IFDIR;
        control_kind(relative, component_text, is_directory)?;
        if is_directory {
            let child =
                open_directory_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
            let relative = if relative.is_empty() {
                component_text.to_owned()
            } else {
                format!("{relative}/{component_text}")
            };
            validate_controls(&child, &relative)?;
            verify_named(directory, &component, Anchor::of(&child.metadata()?))?;
        } else {
            if before_entry.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(C10RepositoryErrorV1::Protection);
            }
            let file = open_at(directory, &component, libc::O_RDONLY, 0)?;
            let metadata = file.metadata()?;
            owned_file(&metadata, false)?;
            if Anchor::stat(&before_entry) != Anchor::of(&metadata) {
                return Err(C10RepositoryErrorV1::Changed);
            }
            if relative.starts_with("refs/") {
                parse_oid_line(&read_file(directory, &component, CONTROL_BYTES, false)?.bytes)?;
            }
            if !same_metadata(&metadata, &file.metadata()?) {
                return Err(C10RepositoryErrorV1::Changed);
            }
            verify_named(directory, &component, Anchor::of(&metadata))?;
        }
    }
    if !same_metadata(&before, &directory.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}
#[cfg(unix)]
fn parse_oid_line(bytes: &[u8]) -> Result<SyncGitObjectIdV1> {
    let text = std::str::from_utf8(bytes).map_err(|_| C10RepositoryErrorV1::Corrupt)?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    SyncGitObjectIdV1::parse(text.to_owned()).map_err(|_| C10RepositoryErrorV1::Corrupt)
}
/// Validate index framing before Git can expand sparse/split-index state.
/// Git's later checksum validation is additional evidence; no Git child reads
/// an index containing sparse-directory modes or mandatory extensions.
#[cfg(unix)]
fn validate_full_index(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 32 || &bytes[..4] != b"DIRC" {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let version = u32::from_be_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| C10RepositoryErrorV1::Protection)?,
    );
    let count = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| C10RepositoryErrorV1::Protection)?,
    ) as usize;
    if !matches!(version, 2..=4) || count > MAX_FILES {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let trailer = bytes.len() - 20;
    let mut offset = 12_usize;
    let mut previous = Vec::<u8>::new();
    for _ in 0..count {
        let start = offset;
        let end = offset
            .checked_add(62)
            .ok_or(C10RepositoryErrorV1::Protection)?;
        if end > trailer {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let mode = u32::from_be_bytes(
            bytes[offset + 24..offset + 28]
                .try_into()
                .map_err(|_| C10RepositoryErrorV1::Protection)?,
        );
        if !matches!(mode, 0o100644 | 0o100755) {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let flags = u16::from_be_bytes(
            bytes[offset + 60..end]
                .try_into()
                .map_err(|_| C10RepositoryErrorV1::Protection)?,
        );
        offset = end;
        if flags & 0x4000 != 0 {
            if version < 3 || offset.checked_add(2).is_none_or(|end| end > trailer) {
                return Err(C10RepositoryErrorV1::Protection);
            }
            let extended = u16::from_be_bytes(
                bytes[offset..offset + 2]
                    .try_into()
                    .map_err(|_| C10RepositoryErrorV1::Protection)?,
            );
            if extended & !0x6000 != 0 {
                return Err(C10RepositoryErrorV1::Protection);
            }
            offset += 2;
        }
        let strip = if version == 4 {
            let mut value = 0_usize;
            let mut first = true;
            loop {
                if offset >= trailer {
                    return Err(C10RepositoryErrorV1::Protection);
                }
                let byte = bytes[offset];
                offset += 1;
                value = if first {
                    (byte & 0x7f) as usize
                } else {
                    value
                        .checked_add(1)
                        .and_then(|value| value.checked_mul(128))
                        .and_then(|value| value.checked_add((byte & 0x7f) as usize))
                        .ok_or(C10RepositoryErrorV1::Protection)?
                };
                first = false;
                if value > previous.len() {
                    return Err(C10RepositoryErrorV1::Protection);
                }
                if byte & 0x80 == 0 {
                    break;
                }
            }
            value
        } else {
            0
        };
        let suffix_end = bytes[offset..trailer]
            .iter()
            .position(|byte| *byte == 0)
            .and_then(|length| offset.checked_add(length))
            .ok_or(C10RepositoryErrorV1::Protection)?;
        let mut path = if version == 4 {
            previous[..previous.len() - strip].to_vec()
        } else {
            Vec::new()
        };
        path.extend_from_slice(&bytes[offset..suffix_end]);
        if path.is_empty()
            || path.len() >= libc::PATH_MAX as usize
            || path[0] == b'/'
            || path.contains(&b'\\')
            || path.iter().any(|byte| byte.is_ascii_control())
            || std::str::from_utf8(&path).is_err()
            || path.split(|byte| *byte == b'/').any(|component| {
                component.is_empty()
                    || component == b"."
                    || component == b".."
                    || component.eq_ignore_ascii_case(b".git")
            })
            || (flags & 0x0fff) as usize != path.len().min(0x0fff)
        {
            return Err(C10RepositoryErrorV1::Protection);
        }
        previous = path;
        offset = suffix_end + 1;
        if version != 4 {
            let padding = (8 - (offset - start) % 8) % 8;
            let end = offset
                .checked_add(padding)
                .ok_or(C10RepositoryErrorV1::Protection)?;
            if end > trailer || bytes[offset..end].iter().any(|byte| *byte != 0) {
                return Err(C10RepositoryErrorV1::Protection);
            }
            offset = end;
        }
    }
    while offset < trailer {
        let header_end = offset
            .checked_add(8)
            .ok_or(C10RepositoryErrorV1::Protection)?;
        if header_end > trailer || !bytes[offset].is_ascii_uppercase() {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let length = u32::from_be_bytes(
            bytes[offset + 4..header_end]
                .try_into()
                .map_err(|_| C10RepositoryErrorV1::Protection)?,
        ) as usize;
        offset = header_end
            .checked_add(length)
            .filter(|end| *end <= trailer)
            .ok_or(C10RepositoryErrorV1::Protection)?;
    }
    if offset != trailer {
        return Err(C10RepositoryErrorV1::Protection);
    }
    Ok(count)
}
#[cfg(any(unix, test))]
fn known_ref(reference: &str) -> bool {
    matches!(reference, "refs/heads/polaris" | "refs/heads/incoming")
        || reference
            .strip_prefix("refs/polaris/recovery/")
            .is_some_and(|name| valid_hex(name, 64))
}
#[cfg(unix)]
fn read_head(repository: &File) -> Result<SyncPriorHeadV2> {
    let head = read_file(repository, OsStr::new("HEAD"), CONTROL_BYTES, false)?;
    if head.bytes != b"ref: refs/heads/polaris\n" {
        return Err(C10RepositoryErrorV1::Corrupt);
    }
    let refs =
        open_directory_at(repository, OsStr::new("refs"))?.ok_or(C10RepositoryErrorV1::Corrupt)?;
    let heads =
        open_directory_at(&refs, OsStr::new("heads"))?.ok_or(C10RepositoryErrorV1::Corrupt)?;
    if stat_at(&heads, OsStr::new("polaris"))?.is_some() {
        return Ok(SyncPriorHeadV2::Present {
            commit: parse_oid_line(
                &read_file(&heads, OsStr::new("polaris"), CONTROL_BYTES, false)?.bytes,
            )?,
        });
    }
    if stat_at(repository, OsStr::new("packed-refs"))?.is_some() {
        let packed = read_file(repository, OsStr::new("packed-refs"), CONTROL_BYTES, false)?;
        let text = std::str::from_utf8(&packed.bytes).map_err(|_| C10RepositoryErrorV1::Corrupt)?;
        let mut found = None;
        let mut seen = BTreeSet::new();
        let mut previous_commit = false;
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            if let Some(oid) = line.strip_prefix('^') {
                if !previous_commit || SyncGitObjectIdV1::parse(oid.to_owned()).is_err() {
                    return Err(C10RepositoryErrorV1::Corrupt);
                }
                previous_commit = false;
                continue;
            }
            let (oid, reference) = line.split_once(' ').ok_or(C10RepositoryErrorV1::Corrupt)?;
            let oid = SyncGitObjectIdV1::parse(oid.to_owned())
                .map_err(|_| C10RepositoryErrorV1::Corrupt)?;
            if !known_ref(reference) || !seen.insert(reference) {
                return Err(C10RepositoryErrorV1::Corrupt);
            }
            if reference == "refs/heads/polaris" {
                found = Some(oid);
            }
            previous_commit = true;
        }
        if let Some(commit) = found {
            return Ok(SyncPriorHeadV2::Present { commit });
        }
    }
    Ok(SyncPriorHeadV2::Unborn)
}
#[cfg(unix)]
fn require_lease(state: &Path, lease: &ProjectMutationLeaseV1) -> Result<()> {
    if lease.store_root() != state.join("rewind-workspace") {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let directory = open_directory_nofollow(lease.store_root())?;
    let metadata = directory.metadata()?;
    owned_directory(&metadata, true)?;
    if (metadata.dev(), metadata.ino()) != lease.store_identity() {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}
#[cfg(unix)]
fn prepared_for_operation(
    state: &Path,
    lease: &ProjectMutationLeaseV1,
    operation: ManagementOperationIdV1,
    request_hash: &str,
    unknown: bool,
) -> Result<(
    SyncLocalPreparedV2,
    crate::sync::remote_intent::SyncIntentIdentityV2,
)> {
    require_lease(state, lease)?;
    let history = C10IntentStoreV2::direct_lookup_at_state_dir(state, lease, operation)
        .map_err(|_| C10RepositoryErrorV1::Corrupt)?
        .ok_or(C10RepositoryErrorV1::Corrupt)?;
    let identity = history.current().identity().clone();
    if identity.operation_id != operation
        || identity.request_hash.as_hex() != request_hash
        || identity
            .request_hash_for_prepared(match history.records()[0].state() {
                SyncRemoteIntentStateV2::LocalPrepared { prepared } => prepared,
                _ => return Err(C10RepositoryErrorV1::Corrupt),
            })
            .map_err(|_| C10RepositoryErrorV1::Corrupt)?
            != identity.request_hash
    {
        return Err(C10RepositoryErrorV1::Corrupt);
    }
    let prepared = match history.records()[0].state() {
        SyncRemoteIntentStateV2::LocalPrepared { prepared } => prepared.as_ref().clone(),
        _ => return Err(C10RepositoryErrorV1::Corrupt),
    };
    if unknown {
        if !matches!(
            history.current().state(),
            SyncRemoteIntentStateV2::OutcomeUnknown { .. }
                | SyncRemoteIntentStateV2::LocalPrepared { .. }
        ) {
            return Err(C10RepositoryErrorV1::Corrupt);
        }
    } else if !matches!(
        history.current().state(),
        SyncRemoteIntentStateV2::LocalPrepared { .. }
    ) {
        return Err(C10RepositoryErrorV1::Corrupt);
    }
    Ok((prepared, identity))
}
#[cfg(unix)]
fn require_c10_effect(request: &crate::management_effect::PrepareManagementEffectV1) -> Result<()> {
    use crate::management_effect::{ManagementPermissionScopeV1, ManagementRewindDispositionV1};
    if request.inventory.encoded() != "C10"
        || request.scope.permission != ManagementPermissionScopeV1::Remote
        || request.rewind != ManagementRewindDispositionV1::IrreversibleRemote
        || request.scope.account_binding.is_none()
    {
        return Err(C10RepositoryErrorV1::Protection);
    }
    Ok(())
}

#[cfg(unix)]
impl C10LocalRepositoryV1 {
    /// Fully absent is separate from damaged/partial controls. No mkdir, chmod,
    /// Git init, index refresh or recovery is hidden in this reader.
    pub(crate) fn inspect(
        state: &Path,
        git: &ExecutableIdentityV1,
    ) -> Result<C10RepositoryInspectionV1> {
        let state = DirectoryPath::pin(state, true)?;
        git.revalidate()
            .map_err(|_| C10RepositoryErrorV1::BeforeSpawn)?;
        let paths = SyncPaths::under(&state.path);
        let Some(sync) = open_directory_at(&state.directory, OsStr::new("sync"))? else {
            state.verify()?;
            if stat_at(&state.directory, OsStr::new("sync"))?.is_some() {
                return Err(C10RepositoryErrorV1::Changed);
            }
            return Ok(C10RepositoryInspectionV1::Absent(C10AbsentRepositoryV1 {
                state,
                git: git.clone(),
            }));
        };
        owned_directory(&sync.metadata()?, false)?;
        let before = sync.metadata()?;
        if stat_at(&sync, OsStr::new("index.lock"))?.is_some() {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let repository = open_directory_at(&sync, OsStr::new("repository"))?;
        let staging = open_directory_at(&sync, OsStr::new("staging"))?;
        let index = stat_at(&sync, OsStr::new("index"))?;
        if repository.is_none() && staging.is_none() && index.is_none() {
            if !same_metadata(&before, &sync.metadata()?) {
                return Err(C10RepositoryErrorV1::Changed);
            }
            state.verify()?;
            verify_named(&state.directory, OsStr::new("sync"), Anchor::of(&before))?;
            return Ok(C10RepositoryInspectionV1::Absent(C10AbsentRepositoryV1 {
                state,
                git: git.clone(),
            }));
        }
        let (Some(repository), Some(staging)) = (repository, staging) else {
            return Err(C10RepositoryErrorV1::Corrupt);
        };
        owned_directory(&repository.metadata()?, false)?;
        owned_directory(&staging.metadata()?, false)?;
        if index.is_some() {
            let index = read_file(&sync, OsStr::new("index"), MAX_TOTAL_BYTES, false)?;
            validate_full_index(&index.bytes)?;
        }
        let config = pin_config(&repository)?;
        let mut projection = ObjectProjection::new();
        scan_controls(&repository, "", false, &mut projection)?;
        read_head(&repository)?;
        let _ = scan_payload(&staging, None)?;
        if !same_metadata(&before, &sync.metadata()?) {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let result = Self {
            sync: DirectoryPath::pin(&state.path.join("sync"), false)?,
            repository: DirectoryPath::pin(&paths.git_dir, false)?,
            staging: DirectoryPath::pin(&paths.work_tree, false)?,
            state,
            paths,
            git: git.clone(),
            config,
        };
        result.verify()?;
        Ok(C10RepositoryInspectionV1::Existing(Box::new(result)))
    }
    fn verify(&self) -> Result<()> {
        self.state.verify()?;
        self.sync.verify()?;
        self.repository.verify()?;
        self.staging.verify()?;
        self.config.verify(&self.repository.directory)?;
        validate_controls(&self.repository.directory, "")?;
        if let Some(index) = stat_at(&self.sync.directory, OsStr::new("index"))? {
            if index.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(C10RepositoryErrorV1::Protection);
            }
            let index = read_file(
                &self.sync.directory,
                OsStr::new("index"),
                MAX_TOTAL_BYTES,
                false,
            )?;
            validate_full_index(&index.bytes)?;
        }
        if stat_at(&self.sync.directory, OsStr::new("index.lock"))?.is_some() {
            return Err(C10RepositoryErrorV1::Protection);
        }
        self.git
            .revalidate()
            .map_err(|_| C10RepositoryErrorV1::BeforeSpawn)
    }
    pub(crate) fn revalidate_controls(&self) -> Result<()> {
        self.verify()
    }
    fn arguments(&self, args: &[OsString]) -> Vec<OsString> {
        self.arguments_at(&self.paths.work_tree, args)
    }
    fn arguments_at(&self, work_tree: &Path, args: &[OsString]) -> Vec<OsString> {
        let mut output = vec![
            "--no-optional-locks".into(),
            "--literal-pathspecs".into(),
            format!("--attr-source={EMPTY_TREE}").into(),
            "--git-dir".into(),
            self.paths.git_dir.clone().into_os_string(),
            "--work-tree".into(),
            work_tree.to_owned().into_os_string(),
        ];
        for config in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "core.symlinks=false",
            "core.filemode=true",
            "core.attributesFile=/dev/null",
            "core.autocrlf=false",
            "core.safecrlf=false",
            "core.ignorecase=false",
            "core.precomposeunicode=false",
            "index.threads=1",
            "credential.helper=",
            "gc.auto=0",
            "maintenance.auto=false",
            "user.name=Polaris",
            "user.email=polaris@localhost",
            REQUIRED_FSYNC,
            "core.fsyncMethod=fsync",
        ] {
            output.push("-c".into());
            output.push(config.into());
        }
        output.extend_from_slice(args);
        output
    }
    fn git_output(&self, args: &[OsString]) -> Result<Vec<u8>> {
        self.verify()?;
        let output = run_bound_local_git(&self.git, &self.arguments(args), &self.paths.index)
            .map_err(|failure| match failure.phase() {
                GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
                GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
            })?;
        self.verify()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        if !output.success() {
            return Err(C10RepositoryErrorV1::GitFailed);
        }
        Ok(output.stdout().to_vec())
    }
    /// Owned synthetic fixtures share the production controls and bound runner.
    /// Nonzero Git exits remain opaque typed output for exact observation tests.
    #[cfg(test)]
    pub(crate) fn run_fixture_child(&self, args: &[OsString]) -> Result<GitChildOutputV1> {
        self.verify()?;
        let output = run_bound_local_git(&self.git, &self.arguments(args), &self.paths.index)
            .map_err(|failure| match failure.phase() {
                GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
                GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
            })?;
        self.verify()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        Ok(output)
    }

    /// Transport supplies typed exact-ref arguments. This does not decide
    /// whether a send/fetch is admissible or perform a network operation itself.
    pub(crate) fn run_network_child(
        &self,
        owner: &GitCredentialOwnerV1,
        material: &GitCredentialMaterialV1,
        args: &[OsString],
    ) -> Result<GitChildOutputV1> {
        self.verify()?;
        if owner.git_identity() != &self.git {
            return Err(C10RepositoryErrorV1::BeforeSpawn);
        }
        let output = owner
            .run_bound_git_output_at_index(&self.arguments(args), material, &self.paths.index)
            .map_err(|failure| match failure.phase() {
                GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
                GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
            })?;
        self.verify()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        Ok(output)
    }
    /// No local ref/index/work-tree write: prove that the lease's expected
    /// remote commit is actually an ancestor of the intended local commit.
    pub(crate) fn require_ancestor(
        &self,
        expected: &SyncGitObjectIdV1,
        intended: &SyncGitObjectIdV1,
    ) -> Result<()> {
        self.git(&[
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", expected.as_str()),
        ])?;
        self.git(&[
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", intended.as_str()),
        ])?;
        self.git(&[
            "merge-base",
            "--is-ancestor",
            expected.as_str(),
            intended.as_str(),
        ])
        .map(drop)
    }
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.git_output(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }
    fn oid_output(&self, args: &[OsString]) -> Result<SyncGitObjectIdV1> {
        parse_oid_line(&self.git_output(args)?)
    }
    fn probe_git_features(&self) -> Result<()> {
        let output =
            self.git_output(&[OsString::from("--version")])
                .map_err(|error| match error {
                    C10RepositoryErrorV1::GitFailed => C10RepositoryErrorV1::Unavailable,
                    other => other,
                })?;
        validate_git_version(&output)
    }
    /// A dirty interrupted work tree does not prevent a guarded, read-only HEAD
    /// observation. This does not weaken snapshot_clean or grant a repair.
    pub(crate) fn head_for_recovery(&self) -> Result<SyncPriorHeadV2> {
        self.verify()?;
        let head = read_head(&self.repository.directory)?;
        self.verify()?;
        if read_head(&self.repository.directory)? != head {
            return Err(C10RepositoryErrorV1::Changed);
        }
        Ok(head)
    }
    fn git_in_recovery_scratch(
        &self,
        scratch: &C10RecoveryScratchV1,
        args: &[OsString],
    ) -> Result<Vec<u8>> {
        self.verify()?;
        scratch.verify()?;
        let output = run_bound_local_git(
            &self.git,
            &self.arguments_at(&scratch.staging.path, args),
            &scratch.index,
        )
        .map_err(|failure| match failure.phase() {
            GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
            GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
        })?;
        self.verify()
            .and_then(|()| scratch.verify())
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        if !output.success() {
            return Err(C10RepositoryErrorV1::GitFailed);
        }
        Ok(output.stdout().to_vec())
    }
    /// Current index and staging are matched to every actual tree mode/blob,
    /// independent of Git's filesystem stat cache and ignored-file behavior.
    pub(crate) fn snapshot_clean(&self) -> Result<C10RepositorySnapshotV1> {
        self.verify()?;
        self.probe_git_features()?;
        let head = read_head(&self.repository.directory)?;
        let (files, _) = scan_payload(&self.staging.directory, None)?;
        let expected_descriptor = descriptor(&files)?;
        match &head {
            SyncPriorHeadV2::Unborn => {
                if !files.is_empty() {
                    return Err(C10RepositoryErrorV1::Dirty);
                }
                if !self.git(&["ls-files", "--stage", "-z"])?.is_empty() {
                    return Err(C10RepositoryErrorV1::Dirty);
                }
            }
            SyncPriorHeadV2::Present { commit } => {
                self.git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])?;
                if !self
                    .git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?
                    .is_empty()
                {
                    return Err(C10RepositoryErrorV1::Dirty);
                }
                self.git(&[
                    "diff-index",
                    "--cached",
                    "--quiet",
                    "--no-ext-diff",
                    "--no-textconv",
                    commit.as_str(),
                    "--",
                ])?;
                self.verify_tree_files(commit, &files)?;
            }
        }
        let again = scan_payload(&self.staging.directory, None)?.0;
        if descriptor(&again)? != expected_descriptor
            || !same_payload_files(&files, &again)
            || read_head(&self.repository.directory)? != head
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.verify()?;
        let index_digest = SyncDigestV1::digest(
            &serde_json::to_vec(&("polaris-c10-logical-index-v1", &expected_descriptor))
                .expect("descriptor"),
        );
        let staging_digest = SyncDigestV1::digest(
            &serde_json::to_vec(&("polaris-c10-logical-staging-v1", &expected_descriptor))
                .expect("descriptor"),
        );
        Ok(C10RepositorySnapshotV1 {
            head,
            descriptor: expected_descriptor,
            index_digest,
            staging_digest,
        })
    }
    fn verify_tree_files(&self, commit: &SyncGitObjectIdV1, files: &[PayloadFile]) -> Result<()> {
        for batch in batches(files)? {
            let mut args = vec![
                "ls-tree".into(),
                "-r".into(),
                "-z".into(),
                "--full-tree".into(),
                commit.as_str().into(),
                "--".into(),
            ];
            args.extend(batch.iter().map(|file| OsString::from(&file.entry.path)));
            let output = self.git_output(&args)?;
            let mut actual = BTreeMap::new();
            for record in output
                .split(|byte| *byte == 0)
                .filter(|record| !record.is_empty())
            {
                let record =
                    std::str::from_utf8(record).map_err(|_| C10RepositoryErrorV1::Corrupt)?;
                let (metadata, path) = record
                    .split_once('\t')
                    .ok_or(C10RepositoryErrorV1::Corrupt)?;
                let fields = metadata.split(' ').collect::<Vec<_>>();
                if fields.len() != 3
                    || fields[1] != "blob"
                    || !matches!(fields[0], "100644" | "100755")
                    || !valid_hex(fields[2], 40)
                    || actual.insert(path, (fields[0], fields[2])).is_some()
                {
                    return Err(C10RepositoryErrorV1::Corrupt);
                }
            }
            if actual.len() != batch.len() {
                return Err(C10RepositoryErrorV1::Dirty);
            }
            for file in batch {
                let expected_mode = match file.entry.mode {
                    SyncPayloadModeV1::Regular => "100644",
                    SyncPayloadModeV1::Executable => "100755",
                };
                if actual.get(file.entry.path.as_str())
                    != Some(&(expected_mode, file.git_oid.as_str()))
                {
                    return Err(C10RepositoryErrorV1::Dirty);
                }
            }
        }
        Ok(())
    }
    /// Decode the actual committed blobs to the private staging tree before
    /// durability. An empty attribute tree disables payload-declared filters,
    /// encodings and line-ending conversion. The SHA256 descriptor is therefore
    /// recomputed from Git's actual blob bytes, including binary/large payloads,
    /// without routing blob contents through the bounded child stdout pipe.
    fn reconstruct_committed_payload(
        &self,
        commit: &SyncGitObjectIdV1,
        expected: &SyncPayloadDescriptorV1,
    ) -> Result<()> {
        if read_head(&self.repository.directory)?
            != (SyncPriorHeadV2::Present {
                commit: commit.clone(),
            })
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])?;
        let empty_tree = self.oid_output(&[
            "hash-object".into(),
            "-t".into(),
            "tree".into(),
            "-w".into(),
            "--".into(),
            "/dev/null".into(),
        ])?;
        if empty_tree.as_str() != EMPTY_TREE {
            return Err(C10RepositoryErrorV1::Corrupt);
        }
        self.git(&["read-tree", "--reset", commit.as_str()])?;
        clear_directory(&self.staging.directory)?;
        self.git_output(&["checkout-index".into(), "--force".into(), "--all".into()])?;
        let files = scan_payload(&self.staging.directory, None)?.0;
        if descriptor(&files)? != *expected {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.verify_tree_files(commit, &files)?;
        if read_head(&self.repository.directory)?
            != (SyncPriorHeadV2::Present {
                commit: commit.clone(),
            })
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.verify()
    }
    fn durable_objects(&self) -> Result<SyncDigestV1> {
        self.verify()?;
        let mut projection = ObjectProjection::new();
        scan_controls(&self.repository.directory, "", true, &mut projection)?;
        if stat_at(&self.sync.directory, OsStr::new("index"))?.is_some() {
            let _ = hash_file(&self.sync.directory, OsStr::new("index"), true)?;
        }
        fsync_payload(&self.staging.directory)?;
        for directory in [
            &self.staging.directory,
            &self.sync.directory,
            &self.state.directory,
        ] {
            directory.sync_all()?;
            fault(FaultPoint::DirectoryFsync)?;
        }
        self.verify()?;
        Ok(projection.finish())
    }
    fn reopen(self) -> Result<Self> {
        let state = self.state.path.clone();
        let git = self.git.clone();
        drop(self);
        fault(FaultPoint::Reopen)?;
        match Self::inspect(&state, &git)? {
            C10RepositoryInspectionV1::Existing(repository) => Ok(*repository),
            C10RepositoryInspectionV1::Absent(_) => Err(C10RepositoryErrorV1::Corrupt),
        }
    }
}

/// A newly created, descriptor-relative private readback tree. Failed
/// verification keeps it as evidence under the still-blocking Unknown effect.
/// It is never an alternate project work tree or a persisted capability.
#[cfg(unix)]
struct C10RecoveryScratchV1 {
    root: DirectoryPath,
    staging: DirectoryPath,
    index: PathBuf,
}
#[cfg(unix)]
impl C10RecoveryScratchV1 {
    fn create(repository: &C10LocalRepositoryV1) -> Result<Self> {
        repository.verify()?;
        let mut nonce = [0_u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| C10RepositoryErrorV1::Unavailable)?;
        let component = format!(
            ".c10-recovery-readback-{}",
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let root = mkdir_at(&repository.state.directory, OsStr::new(&component))?;
        let staging = mkdir_at(&root, OsStr::new("staging"))?;
        let path = repository.state.path.join(&component);
        drop(staging);
        drop(root);
        let value = Self::reopen(&path)?;
        repository.state.verify()?;
        Ok(value)
    }
    fn reopen(path: &Path) -> Result<Self> {
        let root = DirectoryPath::pin(path, true)?;
        let staging = DirectoryPath::pin(&path.join("staging"), true)?;
        let value = Self {
            root,
            staging,
            index: path.join("index"),
        };
        value.verify()?;
        Ok(value)
    }
    fn verify(&self) -> Result<()> {
        self.root.verify()?;
        self.staging.verify()?;
        let before = self.root.directory.metadata()?;
        let mut stream = DirectoryStream::new(&self.root.directory)?;
        while let Some(component) = stream.next()? {
            match component.to_str() {
                Some("staging") => verify_named(
                    &self.root.directory,
                    &component,
                    Anchor::of(&self.staging.directory.metadata()?),
                )?,
                Some("index") => {
                    let index = read_file(&self.root.directory, &component, MAX_TOTAL_BYTES, true)?;
                    validate_full_index(&index.bytes)?;
                }
                _ => return Err(C10RepositoryErrorV1::Protection),
            }
        }
        if !same_metadata(&before, &self.root.directory.metadata()?) {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let _ = scan_payload(&self.staging.directory, None)?;
        Ok(())
    }
    fn verify_payload(&self, expected: &SyncPayloadDescriptorV1) -> Result<Vec<PayloadFile>> {
        self.verify()?;
        let files = scan_payload(&self.staging.directory, None)?.0;
        let index = read_file(
            &self.root.directory,
            OsStr::new("index"),
            MAX_TOTAL_BYTES,
            true,
        )?;
        let count = validate_full_index(&index.bytes)?;
        if count != files.len() || descriptor(&files)? != *expected {
            return Err(C10RepositoryErrorV1::Changed);
        }
        Ok(files)
    }
    fn sync(&self) -> Result<(fs::Metadata, [u8; 32])> {
        self.verify()?;
        let index = hash_file(&self.root.directory, OsStr::new("index"), true)?;
        fsync_payload(&self.staging.directory)?;
        self.root.directory.sync_all()?;
        fault(FaultPoint::DirectoryFsync)?;
        self.root.verify()?;
        self.staging.verify()?;
        Ok(index)
    }
    fn close_and_reopen(self) -> Result<Self> {
        let path = self.root.path.clone();
        drop(self);
        Self::reopen(&path)
    }
    fn remove_verified(self, state: &DirectoryPath) -> Result<()> {
        self.verify()?;
        let component = self
            .root
            .path
            .file_name()
            .ok_or(C10RepositoryErrorV1::Protection)?
            .to_owned();
        let anchor = Anchor::of(&self.root.directory.metadata()?);
        clear_directory(&self.root.directory)?;
        self.root.verify()?;
        verify_named(&state.directory, &component, anchor)?;
        drop(self);
        let component = name(&component)?;
        if unsafe {
            libc::unlinkat(
                state.directory.as_raw_fd(),
                component.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        state.directory.sync_all()?;
        fault(FaultPoint::DirectoryFsync)?;
        state.verify()
    }
}

#[cfg(unix)]
fn same_payload_files(a: &[PayloadFile], b: &[PayloadFile]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.entry == b.entry && a.git_oid == b.git_oid && same_metadata(&a.metadata, &b.metadata)
        })
}
#[cfg(unix)]
fn batches(files: &[PayloadFile]) -> Result<Vec<&[PayloadFile]>> {
    batches_with_prefix(files, 0)
}
#[cfg(unix)]
fn batches_with_prefix(files: &[PayloadFile], prefix: usize) -> Result<Vec<&[PayloadFile]>> {
    let mut output = Vec::new();
    let mut start = 0;
    let mut arguments = 0;
    let mut result = 0;
    for (index, file) in files.iter().enumerate() {
        // hash-object, ls-tree and cacheinfo all have fixed record overhead.
        let arg = file.entry.path.len() + prefix + 100;
        let record = file.entry.path.len() + 64;
        if arg > ARG_BATCH_BYTES || record > OUTPUT_BATCH_BYTES {
            return Err(C10RepositoryErrorV1::Bounds);
        }
        if index > start
            && (arguments + arg > ARG_BATCH_BYTES || result + record > OUTPUT_BATCH_BYTES)
        {
            output.push(&files[start..index]);
            start = index;
            arguments = 0;
            result = 0;
        }
        arguments += arg;
        result += record;
    }
    if start < files.len() {
        output.push(&files[start..]);
    }
    Ok(output)
}

#[cfg(unix)]
fn empty_snapshot() -> C10RepositorySnapshotV1 {
    let descriptor = SyncPayloadDescriptorV1::new(Vec::new()).expect("empty descriptor");
    C10RepositorySnapshotV1 {
        head: SyncPriorHeadV2::Unborn,
        index_digest: SyncDigestV1::digest(
            &serde_json::to_vec(&("polaris-c10-logical-index-v1", &descriptor))
                .expect("descriptor"),
        ),
        staging_digest: SyncDigestV1::digest(
            &serde_json::to_vec(&("polaris-c10-logical-staging-v1", &descriptor))
                .expect("descriptor"),
        ),
        descriptor,
    }
}
/// Source and every Git-controlled path must be disjoint in both directions.
/// The shared SyncPaths helper checks only controls nested in the project.
#[cfg(unix)]
fn same_directory_identity(a: Anchor, b: Anchor) -> bool {
    (a.dev, a.ino) == (b.dev, b.ino)
}
#[cfg(unix)]
fn closest_control_directory(control: &Path) -> Result<(DirectoryPath, bool)> {
    let mut candidate = control;
    loop {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.is_dir() => {
                return Ok((DirectoryPath::pin(candidate, false)?, candidate == control));
            }
            Ok(metadata) if metadata.is_file() && candidate == control => {
                owned_file(&metadata, false)?;
            }
            Ok(_) => return Err(C10RepositoryErrorV1::Protection),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        candidate = candidate.parent().ok_or(C10RepositoryErrorV1::Protection)?;
    }
}
#[cfg(unix)]
fn require_disjoint_project(paths: &SyncPaths, project: &DirectoryPath) -> Result<()> {
    project.verify()?;
    let project_root = Anchor::of(&project.directory.metadata()?);
    for control in [&paths.git_dir, &paths.work_tree, &paths.index] {
        if control.starts_with(&project.path) || project.path.starts_with(control) {
            return Err(C10RepositoryErrorV1::Protection);
        }
        // Distinct canonical strings can identify the same physical directory
        // through a macOS firmlink. Compare held ancestor identities too. A
        // missing control uses its closest existing ancestor only for the
        // source-containing-control direction: siblings remain admissible.
        let (ancestor, exact_root) = closest_control_directory(control)?;
        let ancestor_root = Anchor::of(&ancestor.directory.metadata()?);
        if same_directory_identity(project_root, ancestor_root)
            || ancestor
                .ancestors
                .iter()
                .any(|entry| same_directory_identity(project_root, entry.anchor))
            || exact_root
                && project
                    .ancestors
                    .iter()
                    .any(|entry| same_directory_identity(ancestor_root, entry.anchor))
        {
            return Err(C10RepositoryErrorV1::Protection);
        }
        ancestor.verify()?;
    }
    project.verify()
}
#[cfg(unix)]
fn pin_recovery_project(
    paths: &SyncPaths,
    project: &Path,
    request: &crate::management_effect::PrepareManagementEffectV1,
) -> Result<DirectoryPath> {
    let project = DirectoryPath::pin(project, false)?;
    require_disjoint_project(paths, &project)?;
    if crate::management_effect::ManagementEffectHashV1::digest(
        project.path.as_os_str().as_encoded_bytes(),
    ) != request.scope.project
    {
        return Err(C10RepositoryErrorV1::Changed);
    }
    project.verify()?;
    Ok(project)
}
#[cfg(unix)]
fn require_actual_effect(
    management: &crate::management_effect::ManagementEffectStoreV1,
    state: &Path,
    request: &crate::management_effect::PrepareManagementEffectV1,
    unknown: bool,
) -> Result<()> {
    require_c10_effect(request)?;
    if management.state_dir() != state {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let effect = management
        .effect(request.operation_id)
        .map_err(|_| C10RepositoryErrorV1::Corrupt)?
        .ok_or(C10RepositoryErrorV1::Corrupt)?;
    if effect.request != *request
        || !matches!(
            (&effect.state, unknown),
            (
                crate::management_effect::ManagementEffectStateV1::EffectStarted,
                false
            ) | (
                crate::management_effect::ManagementEffectStateV1::OutcomeUnknown { .. },
                true
            )
        )
    {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}
#[cfg(unix)]
fn require_request_identity(
    request: &crate::management_effect::PrepareManagementEffectV1,
    identity: &crate::sync::remote_intent::SyncIntentIdentityV2,
) -> Result<()> {
    if request.request_hash.as_hex() != identity.request_hash.as_hex()
        || request.scope.project.as_hex() != identity.project.as_hex()
        || request.scope.account_binding
            != Some(
                identity
                    .management_account_binding_hash()
                    .map_err(|_| C10RepositoryErrorV1::Corrupt)?,
            )
    {
        return Err(C10RepositoryErrorV1::Corrupt);
    }
    Ok(())
}
#[cfg(unix)]
fn snapshot_matches_prepared(
    snapshot: &C10RepositorySnapshotV1,
    prepared: &SyncLocalPreparedV2,
) -> Result<()> {
    if snapshot.head != prepared.prior_head
        || snapshot.descriptor != prepared.head_tree
        || snapshot.index_digest != prepared.prior_index_digest
        || snapshot.staging_digest != prepared.prior_staging_digest
    {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}
#[cfg(unix)]
fn mkdir_at(parent: &File, component: &OsStr) -> Result<File> {
    let name = name(component)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let directory = open_directory_at(parent, component)?.ok_or(C10RepositoryErrorV1::Changed)?;
    owned_directory(&directory.metadata()?, true)?;
    directory.sync_all()?;
    parent.sync_all()?;
    fault(FaultPoint::DirectoryFsync)?;
    Ok(directory)
}
#[cfg(unix)]
fn probe_git_features(git: &ExecutableIdentityV1, index: &Path) -> Result<()> {
    let output = run_bound_local_git(
        git,
        &[
            OsString::from(format!("--attr-source={EMPTY_TREE}")),
            OsString::from("--version"),
        ],
        index,
    )
    .map_err(|failure| match failure.phase() {
        GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
        GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
    })?;
    if !output.success() {
        return Err(C10RepositoryErrorV1::Unavailable);
    }
    validate_git_version(output.stdout())
}
#[cfg(unix)]
fn validate_git_version(output: &[u8]) -> Result<()> {
    let version = std::str::from_utf8(output)
        .map_err(|_| C10RepositoryErrorV1::Unavailable)?
        .strip_prefix("git version ")
        .ok_or(C10RepositoryErrorV1::Unavailable)?;
    let mut parts = version.split(['.', ' ']);
    let major = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(C10RepositoryErrorV1::Unavailable)?;
    let minor = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(C10RepositoryErrorV1::Unavailable)?;
    // Git 2.43 added the explicit attribute source used by raw restoration.
    // Actual init/hash/checkout plus all-object fsync are tested on the bound
    // binary; the P0-approved binary identity is independently revalidated.
    if major < 2 || major == 2 && minor < 43 {
        return Err(C10RepositoryErrorV1::Unavailable);
    }
    Ok(())
}
/// A repository-less system directory prevents ambient project config from
/// entering an absent-repository observation. This grants no arbitrary child.
#[cfg(unix)]
struct C10ObservationRootV1 {
    directory: File,
    metadata: fs::Metadata,
}
#[cfg(unix)]
fn require_observation_controls_absent(directory: &File) -> Result<()> {
    // Git searches .git and tests HEAD when detecting a bare repository.
    // Only entry absence is accepted, including for dangling symlinks.
    for component in [".git", "HEAD"] {
        if stat_at(directory, OsStr::new(component))?.is_some() {
            return Err(C10RepositoryErrorV1::Protection);
        }
    }
    Ok(())
}
#[cfg(unix)]
fn observation_root_arguments() -> Vec<OsString> {
    ["-C", "/", "--no-pager", "--no-optional-locks"]
        .into_iter()
        .map(OsString::from)
        .collect()
}
#[cfg(unix)]
impl C10ObservationRootV1 {
    fn inspect() -> Result<Self> {
        let directory = open_directory_nofollow(Path::new("/"))?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != 0
            || metadata.mode() & 0o7022 != 0
            || metadata.nlink() == 0
        {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let value = Self {
            directory,
            metadata,
        };
        value.verify()?;
        Ok(value)
    }
    fn verify(&self) -> Result<()> {
        require_observation_controls_absent(&self.directory)?;
        let named = open_directory_nofollow(Path::new("/"))?;
        if !same_metadata(&self.metadata, &self.directory.metadata()?)
            || !same_metadata(&self.metadata, &named.metadata()?)
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        require_observation_controls_absent(&named)
    }
}

#[cfg(unix)]
impl C10AbsentRepositoryV1 {
    pub(crate) fn snapshot_clean(&self) -> Result<C10RepositorySnapshotV1> {
        self.require_absent()?;
        Ok(empty_snapshot())
    }
    fn require_absent(&self) -> Result<()> {
        self.state.verify()?;
        if let Some(sync) = open_directory_at(&self.state.directory, OsStr::new("sync"))? {
            let before = sync.metadata()?;
            for component in ["repository", "staging", "index", "index.lock"] {
                if stat_at(&sync, OsStr::new(component))?.is_some() {
                    return Err(C10RepositoryErrorV1::Changed);
                }
            }
            if !same_metadata(&before, &sync.metadata()?) {
                return Err(C10RepositoryErrorV1::Changed);
            }
            verify_named(
                &self.state.directory,
                OsStr::new("sync"),
                Anchor::of(&before),
            )?;
        }
        self.state.verify()
    }
    /// Observe only the canonical exact remote ref without creating a local
    /// repository/index or inheriting the caller's ambient Git config.
    pub(crate) fn observe_exact_child(
        &self,
        owner: &GitCredentialOwnerV1,
        binding: &GitCredentialBindingV1,
        remote: &CanonicalHttpsRemoteV1,
    ) -> Result<GitChildOutputV1> {
        self.require_absent()?;
        if CanonicalHttpsRemoteV1::parse(&remote.transport_url())
            .map_err(|_| C10RepositoryErrorV1::BeforeSpawn)?
            != *remote
        {
            return Err(C10RepositoryErrorV1::BeforeSpawn);
        }
        binding
            .validate()
            .map_err(|_| C10RepositoryErrorV1::BeforeSpawn)?;
        if !binding.matches_remote(remote)
            || owner.git_identity() != &self.git
            || binding.git != self.git
        {
            return Err(C10RepositoryErrorV1::BeforeSpawn);
        }
        let root = C10ObservationRootV1::inspect()?;
        let material = owner
            .read_bound_material(binding)
            .map_err(|_| C10RepositoryErrorV1::BeforeSpawn)?;
        // Credential reads may display a native prompt. Recheck both pinned
        // directories after that wait and immediately before the child.
        root.verify()?;
        self.require_absent()?;
        let mut args = observation_root_arguments();
        args.extend([
            OsString::from("ls-remote"),
            OsString::from("--refs"),
            OsString::from("--exit-code"),
            OsString::from("--"),
            OsString::from(remote.transport_url()),
            OsString::from("refs/heads/polaris"),
        ]);
        let output =
            owner
                .run_bound_git_output(&args, &material)
                .map_err(|failure| match failure.phase() {
                    GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
                    GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
                })?;
        root.verify()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        self.require_absent()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        Ok(output)
    }

    /// Test-only exact observation keeps the same fixed root and absence gates.
    /// Production callers must use their actual bound credential owner.
    #[cfg(test)]
    pub(crate) fn observe_exact_fixture_child(
        &self,
        remote: &CanonicalHttpsRemoteV1,
    ) -> Result<GitChildOutputV1> {
        self.require_absent()?;
        if CanonicalHttpsRemoteV1::parse(&remote.transport_url())
            .map_err(|_| C10RepositoryErrorV1::BeforeSpawn)?
            != *remote
        {
            return Err(C10RepositoryErrorV1::BeforeSpawn);
        }
        let root = C10ObservationRootV1::inspect()?;
        let mut args = observation_root_arguments();
        args.extend([
            OsString::from("ls-remote"),
            OsString::from("--refs"),
            OsString::from("--exit-code"),
            OsString::from("--"),
            OsString::from(remote.transport_url()),
            OsString::from("refs/heads/polaris"),
        ]);
        root.verify()?;
        self.require_absent()?;
        let output =
            run_bound_local_git(&self.git, &args, &SyncPaths::under(&self.state.path).index)
                .map_err(|failure| match failure.phase() {
                    GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
                    GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
                })?;
        root.verify()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        self.require_absent()
            .map_err(|_| C10RepositoryErrorV1::AfterSpawn)?;
        Ok(output)
    }

    fn prepared_for_absent_restore(
        &self,
        management: &crate::management_effect::ManagementEffectStoreV1,
        unknown: &UnknownManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(
        SyncLocalPreparedV2,
        crate::sync::remote_intent::SyncIntentIdentityV2,
    )> {
        require_actual_effect(management, &self.state.path, unknown.request(), true)?;
        let (prepared, identity) = prepared_for_operation(
            &self.state.path,
            lease,
            unknown.operation_id(),
            &unknown.request().request_hash.as_hex(),
            true,
        )?;
        require_request_identity(unknown.request(), &identity)?;
        if identity.account_binding.git != self.git || prepared.agreed_commit.is_some() {
            return Err(C10RepositoryErrorV1::Changed);
        }
        snapshot_matches_prepared(&self.snapshot_clean()?, &prepared)?;
        let history = C10IntentStoreV2::direct_lookup_at_state_dir(
            &self.state.path,
            lease,
            unknown.operation_id(),
        )
        .map_err(|_| C10RepositoryErrorV1::Corrupt)?
        .ok_or(C10RepositoryErrorV1::Corrupt)?;
        let SyncRemoteIntentStateV2::OutcomeUnknown { unknown: stored } = history.current().state()
        else {
            return Err(C10RepositoryErrorV1::Corrupt);
        };
        if history.current().identity() != &identity
            || stored.previous_state
                != crate::sync::remote_intent::SyncPreviousStateV2::LocalPrepared
            || stored.intended_commit.is_some()
            || stored.created_commit_proof.is_some()
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        Ok((prepared, identity))
    }

    /// An interrupted LP before initialization can abandon its still-absent
    /// local state. Only existing parents are synced; no Git child or control
    /// creation is part of this actual close/reopen restoration capability.
    pub(crate) fn restore_absent_after_unknown(
        self,
        project: &Path,
        management: &crate::management_effect::ManagementEffectStoreV1,
        unknown: &UnknownManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<C10RestoreReadbackV1> {
        let project = pin_recovery_project(
            &SyncPaths::under(&self.state.path),
            project,
            unknown.request(),
        )?;
        let (prepared, identity) = self.prepared_for_absent_restore(management, unknown, lease)?;
        let before = self.snapshot_clean()?;
        self.require_absent()?;
        if let Some(sync) = open_directory_at(&self.state.directory, OsStr::new("sync"))? {
            let anchor = Anchor::of(&sync.metadata()?);
            sync.sync_all()?;
            fault(FaultPoint::DirectoryFsync)?;
            verify_named(&self.state.directory, OsStr::new("sync"), anchor)?;
        }
        self.state.directory.sync_all()?;
        if let Some(parent) = self.state.ancestors.last() {
            parent.parent.sync_all()?;
        }
        fault(FaultPoint::DirectoryFsync)?;
        project.verify()?;
        self.require_absent()?;
        let state = self.state.path.clone();
        let state_anchor = Anchor::of(&self.state.directory.metadata()?);
        let git = self.git.clone();
        drop(self);
        fault(FaultPoint::Reopen)?;
        let reopened = match C10LocalRepositoryV1::inspect(&state, &git)? {
            C10RepositoryInspectionV1::Absent(absent) => absent,
            _ => return Err(C10RepositoryErrorV1::Changed),
        };
        if Anchor::of(&reopened.state.directory.metadata()?) != state_anchor {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let (again_prepared, again_identity) =
            reopened.prepared_for_absent_restore(management, unknown, lease)?;
        if again_prepared != prepared || again_identity != identity {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let restored = reopened.snapshot_clean()?;
        if restored.head != before.head
            || restored.descriptor != before.descriptor
            || restored.index_digest != before.index_digest
            || restored.staging_digest != before.staging_digest
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        project.verify()?;
        require_disjoint_project(&SyncPaths::under(&state), &project)?;
        reopened.require_absent()?;
        fault(FaultPoint::Readback)?;
        Ok(C10RestoreReadbackV1 {
            operation: identity.operation_id,
            request_hash: identity.request_hash,
            expected_current: before.head,
            restored: restored.head,
            descriptor: restored.descriptor,
            index_digest: restored.index_digest,
            staging_digest: restored.staging_digest,
            objects_digest: ObjectProjection::new().finish(),
        })
    }

    pub(crate) fn initialize_after_local_prepared(
        self,
        management: &crate::management_effect::ManagementEffectStoreV1,
        started: &StartedManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
        payload: &C10PayloadSnapshotV1,
    ) -> Result<C10LocalRepositoryV1> {
        require_disjoint_project(&SyncPaths::under(&self.state.path), &payload.project)?;
        require_actual_effect(management, &self.state.path, started.request(), false)?;
        let (prepared, identity) = prepared_for_operation(
            &self.state.path,
            lease,
            started.operation_id(),
            &started.request().request_hash.as_hex(),
            false,
        )?;
        require_request_identity(started.request(), &identity)?;
        snapshot_matches_prepared(&self.snapshot_clean()?, &prepared)?;
        self.require_absent()?;
        let paths = SyncPaths::under(&self.state.path);
        if payload.descriptor() != &identity.payload
            || crate::management_effect::ManagementEffectHashV1::digest(
                payload.project.path.as_os_str().as_encoded_bytes(),
            ) != started.request().scope.project
        {
            return Err(C10RepositoryErrorV1::Protection);
        }
        payload.revalidate()?;
        probe_git_features(&self.git, &paths.index)?;
        let sync = match open_directory_at(&self.state.directory, OsStr::new("sync"))? {
            Some(directory) => directory,
            None => mkdir_at(&self.state.directory, OsStr::new("sync"))?,
        };
        let repository = mkdir_at(&sync, OsStr::new("repository"))?;
        let staging = mkdir_at(&sync, OsStr::new("staging"))?;
        self.state.verify()?;
        verify_named(
            &sync,
            OsStr::new("repository"),
            Anchor::of(&repository.metadata()?),
        )?;
        verify_named(
            &sync,
            OsStr::new("staging"),
            Anchor::of(&staging.metadata()?),
        )?;
        let mut args = vec![
            "--git-dir".into(),
            paths.git_dir.clone().into_os_string(),
            "--work-tree".into(),
            paths.work_tree.clone().into_os_string(),
        ];
        for value in [
            REQUIRED_FSYNC,
            "core.fsyncMethod=fsync",
            "core.hooksPath=/dev/null",
            "index.threads=1",
            "gc.auto=0",
        ] {
            args.push("-c".into());
            args.push(value.into());
        }
        args.extend(
            ["init", "--quiet", "--template=", "--initial-branch=polaris"]
                .into_iter()
                .map(OsString::from),
        );
        let output =
            run_bound_local_git(&self.git, &args, &paths.index).map_err(|failure| match failure
                .phase()
            {
                GitChildFailurePhaseV1::BeforeSpawn => C10RepositoryErrorV1::BeforeSpawn,
                GitChildFailurePhaseV1::AfterSpawn => C10RepositoryErrorV1::AfterSpawn,
            })?;
        if !output.success() {
            return Err(C10RepositoryErrorV1::GitFailed);
        }
        fault(FaultPoint::RawInitialization)?;
        verify_named(
            &sync,
            OsStr::new("repository"),
            Anchor::of(&repository.metadata()?),
        )?;
        verify_named(
            &sync,
            OsStr::new("staging"),
            Anchor::of(&staging.metadata()?),
        )?;
        // Git may emit core.worktree for this separate work tree. No child
        // consumes that generated config after init: pin its physical file,
        // replace it with our closed config, then apply the ordinary strict
        // reader. Existing repositories never take this normalization path.
        let config = read_file(&repository, OsStr::new("config"), CONFIG_BYTES, false)?;
        let mut file = open_at(&repository, OsStr::new("config"), libc::O_WRONLY, 0)?;
        if !same_metadata(&file.metadata()?, &config.metadata) {
            return Err(C10RepositoryErrorV1::Changed);
        }
        file.set_len(0)?;
        file.write_all(b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n")?;
        file.sync_all()?;
        fault(FaultPoint::FileFsync)?;
        repository.sync_all()?;
        staging.sync_all()?;
        sync.sync_all()?;
        self.state.directory.sync_all()?;
        verify_named(
            &repository,
            OsStr::new("config"),
            Anchor::of(&file.metadata()?),
        )?;
        pin_config(&repository)?;
        fault(FaultPoint::Initialization)?;
        let state = self.state.path.clone();
        let git = self.git.clone();
        drop(file);
        drop(repository);
        drop(staging);
        drop(sync);
        drop(self);
        match C10LocalRepositoryV1::inspect(&state, &git)? {
            C10RepositoryInspectionV1::Existing(repository) => Ok(*repository),
            _ => Err(C10RepositoryErrorV1::Corrupt),
        }
    }
}
#[cfg(unix)]
impl C10LocalRepositoryV1 {
    pub(crate) fn commit_after_local_prepared(
        self,
        management: &crate::management_effect::ManagementEffectStoreV1,
        started: &StartedManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
        payload: &C10PayloadSnapshotV1,
    ) -> Result<(Self, C10CommitReadbackV1)> {
        require_disjoint_project(&self.paths, &payload.project)?;
        require_actual_effect(management, &self.state.path, started.request(), false)?;
        let (prepared, identity) = prepared_for_operation(
            &self.state.path,
            lease,
            started.operation_id(),
            &started.request().request_hash.as_hex(),
            false,
        )?;
        require_request_identity(started.request(), &identity)?;
        if crate::management_effect::ManagementEffectHashV1::digest(
            payload.project.path.as_os_str().as_encoded_bytes(),
        ) != started.request().scope.project
            || payload.descriptor() != &identity.payload
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        payload.revalidate()?;
        snapshot_matches_prepared(&self.snapshot_clean()?, &prepared)?;
        self.probe_git_features()?;
        let (commit, created) = match &prepared.prior_head {
            SyncPriorHeadV2::Present { commit } if prepared.head_tree == identity.payload => {
                (commit.clone(), false)
            }
            _ => {
                clear_directory(&self.staging.directory)?;
                copy_payload(payload, &self.staging.directory)?;
                if descriptor(&scan_payload(&self.staging.directory, None)?.0)? != identity.payload
                {
                    return Err(C10RepositoryErrorV1::Changed);
                }
                fault(FaultPoint::Staging)?;
                self.git(&["read-tree", "--empty"])?;
                for batch in batches_with_prefix(
                    &payload.files,
                    self.paths.work_tree.as_os_str().as_bytes().len() + 1,
                )? {
                    let mut args = vec![
                        "hash-object".into(),
                        "-w".into(),
                        "--no-filters".into(),
                        "--".into(),
                    ];
                    args.extend(
                        batch.iter().map(|file| {
                            self.paths.work_tree.join(&file.entry.path).into_os_string()
                        }),
                    );
                    if args
                        .iter()
                        .map(|arg| arg.as_bytes().len() + 1)
                        .sum::<usize>()
                        > ARG_BATCH_BYTES
                    {
                        return Err(C10RepositoryErrorV1::Bounds);
                    }
                    let output = self.git_output(&args)?;
                    let oids = std::str::from_utf8(&output)
                        .map_err(|_| C10RepositoryErrorV1::Corrupt)?
                        .lines()
                        .collect::<Vec<_>>();
                    if oids.len() != batch.len() {
                        return Err(C10RepositoryErrorV1::Corrupt);
                    }
                    let mut index_args = vec!["update-index".into(), "--add".into()];
                    for (file, oid) in batch.iter().zip(oids) {
                        if oid != file.git_oid {
                            return Err(C10RepositoryErrorV1::Changed);
                        }
                        let mode = match file.entry.mode {
                            SyncPayloadModeV1::Regular => "100644",
                            SyncPayloadModeV1::Executable => "100755",
                        };
                        index_args.push("--cacheinfo".into());
                        index_args.push(format!("{mode},{oid},{}", file.entry.path).into());
                    }
                    self.git_output(&index_args)?;
                }
                fault(FaultPoint::Index)?;
                payload.revalidate()?;
                if descriptor(&scan_payload(&self.staging.directory, None)?.0)? != identity.payload
                {
                    return Err(C10RepositoryErrorV1::Changed);
                }
                let tree = self.oid_output(&[OsString::from("write-tree")])?;
                let staged = scan_payload(&self.staging.directory, None)?.0;
                self.verify_tree_files(&tree, &staged)?;
                let mut args = vec![
                    "commit-tree".into(),
                    tree.as_str().into(),
                    "-m".into(),
                    "Polaris sync".into(),
                ];
                if let SyncPriorHeadV2::Present { commit } = &prepared.prior_head {
                    args.push("-p".into());
                    args.push(commit.as_str().into());
                }
                let commit = self.oid_output(&args)?;
                fault(FaultPoint::CommitObject)?;
                let expected = match &prepared.prior_head {
                    SyncPriorHeadV2::Unborn => "0".repeat(40),
                    SyncPriorHeadV2::Present { commit } => commit.as_str().to_owned(),
                };
                self.git_output(&[
                    "update-ref".into(),
                    "refs/heads/polaris".into(),
                    commit.as_str().into(),
                    expected.into(),
                ])?;
                fault(FaultPoint::Reference)?;
                (commit, true)
            }
        };
        self.reconstruct_committed_payload(&commit, &identity.payload)?;
        let objects_digest = self.durable_objects()?;
        let repository = self.reopen()?;
        repository.git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])?;
        let snapshot = repository.snapshot_clean()?;
        if snapshot.head
            != (SyncPriorHeadV2::Present {
                commit: commit.clone(),
            })
            || snapshot.descriptor != identity.payload
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let mut reopened = ObjectProjection::new();
        scan_controls(&repository.repository.directory, "", false, &mut reopened)?;
        if reopened.finish() != objects_digest {
            return Err(C10RepositoryErrorV1::Changed);
        }
        fault(FaultPoint::Readback)?;
        Ok((
            repository,
            C10CommitReadbackV1 {
                operation: identity.operation_id,
                request_hash: identity.request_hash,
                commit,
                prior_head: prepared.prior_head,
                descriptor: snapshot.descriptor,
                objects_digest,
                created,
            },
        ))
    }

    /// A crash after commit/ref publication but before BeforeSend can obtain
    /// evidence only by repeating actual fsync/reopen, never by a persisted DTO.
    pub(crate) fn durable_current_after_unknown(
        self,
        project: &Path,
        management: &crate::management_effect::ManagementEffectStoreV1,
        unknown: &UnknownManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(Self, C10CommitReadbackV1)> {
        let project = pin_recovery_project(&self.paths, project, unknown.request())?;
        require_actual_effect(management, &self.state.path, unknown.request(), true)?;
        let (prepared, identity) = prepared_for_operation(
            &self.state.path,
            lease,
            unknown.operation_id(),
            &unknown.request().request_hash.as_hex(),
            true,
        )?;
        require_request_identity(unknown.request(), &identity)?;
        let snapshot = self.snapshot_clean()?;
        let SyncPriorHeadV2::Present { commit } = snapshot.head else {
            return Err(C10RepositoryErrorV1::Corrupt);
        };
        if snapshot.descriptor != identity.payload {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.verify_commit_parent(&commit, &prepared.prior_head)?;
        self.probe_git_features()?;
        project.verify()?;
        self.reconstruct_committed_payload(&commit, &identity.payload)?;
        let objects_digest = self.durable_objects()?;
        let repository = self.reopen()?;
        let readback = repository.snapshot_clean()?;
        if readback.head
            != (SyncPriorHeadV2::Present {
                commit: commit.clone(),
            })
            || readback.descriptor != identity.payload
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let mut reopened = ObjectProjection::new();
        scan_controls(&repository.repository.directory, "", false, &mut reopened)?;
        if reopened.finish() != objects_digest {
            return Err(C10RepositoryErrorV1::Changed);
        }
        fault(FaultPoint::Readback)?;
        let created = prepared.prior_head
            != (SyncPriorHeadV2::Present {
                commit: commit.clone(),
            });
        Ok((
            repository,
            C10CommitReadbackV1 {
                operation: identity.operation_id,
                request_hash: identity.request_hash,
                commit,
                prior_head: prepared.prior_head,
                descriptor: readback.descriptor,
                objects_digest,
                created,
            },
        ))
    }
    /// An owner-minted exact observation/fetch cap permits object verification,
    /// never receipt publication. Fetched blobs are checked in an isolated
    /// index/work tree before any primary HEAD/index/staging change.
    pub(crate) fn repair_fetched_after_unknown(
        self,
        project: &Path,
        management: &crate::management_effect::ManagementEffectStoreV1,
        unknown: &UnknownManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
        expected_current: &SyncPriorHeadV2,
        fetched: C10ExactRecoveryFetchV1,
    ) -> Result<(Self, C10CommitReadbackV1)> {
        let project = pin_recovery_project(&self.paths, project, unknown.request())?;
        let (prepared, identity) =
            require_fetched_recovery_intent(&self, management, unknown, lease, &fetched)?;
        let commit = fetched.intended().clone();
        let intended_head = SyncPriorHeadV2::Present {
            commit: commit.clone(),
        };
        if expected_current != &prepared.prior_head && expected_current != &intended_head {
            return Err(C10RepositoryErrorV1::Changed);
        }
        if self.head_for_recovery()? != *expected_current {
            return Err(C10RepositoryErrorV1::Changed);
        }
        self.probe_git_features()?;
        require_recovery_ref(&self, &fetched)?;
        self.git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])?;
        self.verify_commit_parent(&commit, &prepared.prior_head)?;
        project.verify()?;
        let scratch = C10RecoveryScratchV1::create(&self)?;
        let scratch_paths = SyncPaths {
            git_dir: self.paths.git_dir.clone(),
            work_tree: scratch.staging.path.clone(),
            index: scratch.index.clone(),
        };
        require_disjoint_project(&scratch_paths, &project)?;
        let empty_tree = self.oid_output(&[
            "hash-object".into(),
            "-t".into(),
            "tree".into(),
            "-w".into(),
            "--".into(),
            "/dev/null".into(),
        ])?;
        if empty_tree.as_str() != EMPTY_TREE {
            return Err(C10RepositoryErrorV1::Corrupt);
        }
        self.git_in_recovery_scratch(
            &scratch,
            &["read-tree".into(), "--reset".into(), commit.as_str().into()],
        )?;
        self.git_in_recovery_scratch(
            &scratch,
            &["checkout-index".into(), "--force".into(), "--all".into()],
        )?;
        let files = scratch.verify_payload(&identity.payload)?;
        self.verify_tree_files(&commit, &files)?;
        let index = scratch.sync()?;
        let objects_digest = self.durable_objects()?;
        let scratch = scratch.close_and_reopen()?;
        let repository = self.reopen()?;
        let reopened_files = scratch.verify_payload(&identity.payload)?;
        if !same_payload_files(&files, &reopened_files) {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let reopened_index = hash_file(&scratch.root.directory, OsStr::new("index"), false)?;
        if !same_metadata(&index.0, &reopened_index.0) || index.1 != reopened_index.1 {
            return Err(C10RepositoryErrorV1::Changed);
        }
        require_recovery_ref(&repository, &fetched)?;
        repository.git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])?;
        repository.verify_commit_parent(&commit, &prepared.prior_head)?;
        repository.verify_tree_files(&commit, &reopened_files)?;
        let mut projection = ObjectProjection::new();
        scan_controls(&repository.repository.directory, "", false, &mut projection)?;
        if projection.finish() != objects_digest {
            return Err(C10RepositoryErrorV1::Changed);
        }
        fault(FaultPoint::FetchedReadback)?;
        let (again_prepared, again_identity) =
            require_fetched_recovery_intent(&repository, management, unknown, lease, &fetched)?;
        if again_prepared != prepared
            || again_identity != identity
            || repository.head_for_recovery()? != *expected_current
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        project.verify()?;
        let expected_oid = match expected_current {
            SyncPriorHeadV2::Unborn => "0".repeat(40),
            SyncPriorHeadV2::Present { commit } => commit.as_str().to_owned(),
        };
        repository.git_output(&[
            "update-ref".into(),
            "refs/heads/polaris".into(),
            commit.as_str().into(),
            expected_oid.into(),
        ])?;
        fault(FaultPoint::RecoveryReference)?;
        repository.reconstruct_committed_payload(&commit, &identity.payload)?;
        // Remove only this verified, newly owned readback tree. No object GC or
        // unknown temporary cleanup is part of recovery.
        scratch.remove_verified(&repository.state)?;
        let objects_digest = repository.durable_objects()?;
        let repository = repository.reopen()?;
        let readback = repository.snapshot_clean()?;
        if readback.head != intended_head || readback.descriptor != identity.payload {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let mut projection = ObjectProjection::new();
        scan_controls(&repository.repository.directory, "", false, &mut projection)?;
        if projection.finish() != objects_digest {
            return Err(C10RepositoryErrorV1::Changed);
        }
        fault(FaultPoint::Readback)?;
        let created = prepared.prior_head != intended_head;
        Ok((
            repository,
            C10CommitReadbackV1 {
                operation: identity.operation_id,
                request_hash: identity.request_hash,
                commit,
                prior_head: prepared.prior_head,
                descriptor: readback.descriptor,
                objects_digest,
                created,
            },
        ))
    }

    fn verify_commit_parent(
        &self,
        commit: &SyncGitObjectIdV1,
        prior: &SyncPriorHeadV2,
    ) -> Result<()> {
        let output = self.git(&["rev-list", "--parents", "-n", "1", commit.as_str()])?;
        let text = std::str::from_utf8(&output).map_err(|_| C10RepositoryErrorV1::Corrupt)?;
        let fields = text.split_ascii_whitespace().collect::<Vec<_>>();
        let matches = match prior {
            SyncPriorHeadV2::Unborn => fields == [commit.as_str()],
            SyncPriorHeadV2::Present { commit: parent } => {
                fields == [commit.as_str(), parent.as_str()]
            }
        };
        if !matches
            && prior
                != &(SyncPriorHeadV2::Present {
                    commit: commit.clone(),
                })
        {
            return Err(C10RepositoryErrorV1::Corrupt);
        }
        Ok(())
    }

    pub(crate) fn restore_after_unknown(
        self,
        project: &Path,
        management: &crate::management_effect::ManagementEffectStoreV1,
        unknown: &UnknownManagementEffectV1,
        lease: &ProjectMutationLeaseV1,
        expected_current: &SyncPriorHeadV2,
    ) -> Result<(Self, C10RestoreReadbackV1)> {
        let project = pin_recovery_project(&self.paths, project, unknown.request())?;
        require_actual_effect(management, &self.state.path, unknown.request(), true)?;
        let (prepared, identity) = prepared_for_operation(
            &self.state.path,
            lease,
            unknown.operation_id(),
            &unknown.request().request_hash.as_hex(),
            true,
        )?;
        require_request_identity(unknown.request(), &identity)?;
        let history = C10IntentStoreV2::direct_lookup_at_state_dir(
            &self.state.path,
            lease,
            unknown.operation_id(),
        )
        .map_err(|_| C10RepositoryErrorV1::Corrupt)?
        .ok_or(C10RepositoryErrorV1::Corrupt)?;
        let SyncRemoteIntentStateV2::OutcomeUnknown { unknown: stored } = history.current().state()
        else {
            return Err(C10RepositoryErrorV1::Corrupt);
        };
        let bound = stored
            .created_commit_proof
            .as_ref()
            .map(|proof| proof.commit())
            .or(stored.intended_commit.as_ref());
        let expected = match bound {
            Some(commit) => SyncPriorHeadV2::Present {
                commit: commit.clone(),
            },
            None => prepared.prior_head.clone(),
        };
        if *expected_current != expected
            || read_head(&self.repository.directory)? != *expected_current
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        if let SyncPriorHeadV2::Present { commit } = expected_current {
            self.git(&["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())])?;
            if *expected_current != prepared.prior_head {
                self.verify_commit_parent(commit, &prepared.prior_head)?;
            }
        }
        self.probe_git_features()?;
        let expected_oid = match expected_current {
            SyncPriorHeadV2::Unborn => "0".repeat(40),
            SyncPriorHeadV2::Present { commit } => commit.as_str().to_owned(),
        };
        project.verify()?;
        match &prepared.prior_head {
            SyncPriorHeadV2::Unborn => {
                self.git_output(&[
                    "update-ref".into(),
                    "-d".into(),
                    "refs/heads/polaris".into(),
                    expected_oid.into(),
                ])?;
                self.git(&["read-tree", "--empty"])?;
                clear_directory(&self.staging.directory)?;
            }
            SyncPriorHeadV2::Present { commit } => {
                self.git_output(&[
                    "update-ref".into(),
                    "refs/heads/polaris".into(),
                    commit.as_str().into(),
                    expected_oid.into(),
                ])?;
                self.reconstruct_committed_payload(commit, &prepared.head_tree)?;
            }
        }
        fault(FaultPoint::Restoration)?;
        let objects_digest = self.durable_objects()?;
        let repository = self.reopen()?;
        let restored = repository.snapshot_clean()?;
        snapshot_matches_prepared(&restored, &prepared)?;
        let mut projection = ObjectProjection::new();
        scan_controls(&repository.repository.directory, "", false, &mut projection)?;
        if projection.finish() != objects_digest {
            return Err(C10RepositoryErrorV1::Changed);
        }
        fault(FaultPoint::Readback)?;
        Ok((
            repository,
            C10RestoreReadbackV1 {
                operation: identity.operation_id,
                request_hash: identity.request_hash,
                expected_current: expected_current.clone(),
                restored: restored.head,
                descriptor: restored.descriptor,
                index_digest: restored.index_digest,
                staging_digest: restored.staging_digest,
                objects_digest,
            },
        ))
    }
}
#[cfg(unix)]
fn require_recovery_ref(
    repository: &C10LocalRepositoryV1,
    fetched: &C10ExactRecoveryFetchV1,
) -> Result<()> {
    let reference = fetched.recovery_ref();
    if !known_ref(&reference)
        || repository.oid_output(&["rev-parse".into(), "--verify".into(), reference.into()])?
            != *fetched.intended()
    {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}
#[cfg(unix)]
fn require_fetched_recovery_intent(
    repository: &C10LocalRepositoryV1,
    management: &crate::management_effect::ManagementEffectStoreV1,
    unknown: &UnknownManagementEffectV1,
    lease: &ProjectMutationLeaseV1,
    fetched: &C10ExactRecoveryFetchV1,
) -> Result<(
    SyncLocalPreparedV2,
    crate::sync::remote_intent::SyncIntentIdentityV2,
)> {
    require_actual_effect(management, &repository.state.path, unknown.request(), true)?;
    let (prepared, identity) = prepared_for_operation(
        &repository.state.path,
        lease,
        unknown.operation_id(),
        &unknown.request().request_hash.as_hex(),
        true,
    )?;
    require_request_identity(unknown.request(), &identity)?;
    if &identity != fetched.identity() || identity.account_binding.git != repository.git {
        return Err(C10RepositoryErrorV1::Changed);
    }
    let history = C10IntentStoreV2::direct_lookup_at_state_dir(
        &repository.state.path,
        lease,
        unknown.operation_id(),
    )
    .map_err(|_| C10RepositoryErrorV1::Corrupt)?
    .ok_or(C10RepositoryErrorV1::Corrupt)?;
    if history.current().identity() != &identity {
        return Err(C10RepositoryErrorV1::Changed);
    }
    let SyncRemoteIntentStateV2::OutcomeUnknown { unknown: stored } = history.current().state()
    else {
        return Err(C10RepositoryErrorV1::Changed);
    };
    let bound = stored
        .created_commit_proof
        .as_ref()
        .map(|proof| proof.commit())
        .or(stored.intended_commit.as_ref())
        .or_else(|| match &prepared.prior_head {
            SyncPriorHeadV2::Present { commit } if prepared.head_tree == identity.payload => {
                Some(commit)
            }
            _ => None,
        })
        .ok_or(C10RepositoryErrorV1::Corrupt)?;
    if bound != fetched.intended() {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok((prepared, identity))
}

#[cfg(unix)]
fn open_relative_parent(root: &File, path: &str, create: bool) -> Result<(File, OsString)> {
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || matches!(*part, "." | ".."))
    {
        return Err(C10RepositoryErrorV1::Protection);
    }
    let mut parent = root.try_clone()?;
    for component in &parts[..parts.len() - 1] {
        parent = match open_directory_at(&parent, OsStr::new(component))? {
            Some(directory) => directory,
            None if create => mkdir_at(&parent, OsStr::new(component))?,
            None => return Err(C10RepositoryErrorV1::Changed),
        };
    }
    Ok((parent, OsString::from(parts[parts.len() - 1])))
}
#[cfg(unix)]
fn copy_payload(payload: &C10PayloadSnapshotV1, staging: &File) -> Result<()> {
    payload.revalidate()?;
    for record in &payload.files {
        let (source_parent, source_name) =
            open_relative_parent(&payload.project.directory, &record.entry.path, false)?;
        let source = read_file(&source_parent, &source_name, MAX_FILE_BYTES, false)?;
        if !same_metadata(&record.metadata, &source.metadata)
            || SyncDigestV1::digest(&source.bytes) != record.entry.content_hash
            || git_blob_oid_from_bytes(&source.bytes) != record.git_oid
        {
            return Err(C10RepositoryErrorV1::Changed);
        }
        let (target_parent, target_name) = open_relative_parent(staging, &record.entry.path, true)?;
        let mode = match record.entry.mode {
            SyncPayloadModeV1::Regular => 0o600,
            SyncPayloadModeV1::Executable => 0o700,
        };
        let mut target = open_at(
            &target_parent,
            &target_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            mode,
        )?;
        owned_file(&target.metadata()?, false)?;
        target.write_all(&source.bytes)?;
        target.sync_all()?;
        fault(FaultPoint::FileFsync)?;
        verify_named(
            &target_parent,
            &target_name,
            Anchor::of(&target.metadata()?),
        )?;
        target_parent.sync_all()?;
        fault(FaultPoint::DirectoryFsync)?;
    }
    payload.revalidate()
}
#[cfg(unix)]
fn clear_directory(directory: &File) -> Result<()> {
    owned_directory(&directory.metadata()?, false)?;
    let mut stream = DirectoryStream::new(directory)?;
    while let Some(component) = stream.next()? {
        let stat = stat_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
        let directory_kind = stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if directory_kind {
            let child =
                open_directory_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
            clear_directory(&child)?;
            verify_named(directory, &component, Anchor::of(&child.metadata()?))?;
        } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
            let file = open_at(directory, &component, libc::O_RDONLY, 0)?;
            owned_file(&file.metadata()?, false)?;
            verify_named(directory, &component, Anchor::of(&file.metadata()?))?;
        } else {
            return Err(C10RepositoryErrorV1::Protection);
        }
        let component = name(&component)?;
        if unsafe {
            libc::unlinkat(
                directory.as_raw_fd(),
                component.as_ptr(),
                if directory_kind {
                    libc::AT_REMOVEDIR
                } else {
                    0
                },
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        directory.sync_all()?;
        fault(FaultPoint::DirectoryFsync)?;
    }
    Ok(())
}
#[cfg(unix)]
fn fsync_payload(directory: &File) -> Result<()> {
    let before = directory.metadata()?;
    owned_directory(&before, false)?;
    let mut stream = DirectoryStream::new(directory)?;
    while let Some(component) = stream.next()? {
        let stat = stat_at(directory, &component)?.ok_or(C10RepositoryErrorV1::Changed)?;
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child = open_directory_at(directory, &component)?
                    .ok_or(C10RepositoryErrorV1::Changed)?;
                fsync_payload(&child)?;
                verify_named(directory, &component, Anchor::of(&child.metadata()?))?;
            }
            libc::S_IFREG => {
                let _ = hash_file(directory, &component, true)?;
            }
            _ => return Err(C10RepositoryErrorV1::Protection),
        }
    }
    directory.sync_all()?;
    fault(FaultPoint::DirectoryFsync)?;
    if !same_metadata(&before, &directory.metadata()?) {
        return Err(C10RepositoryErrorV1::Changed);
    }
    Ok(())
}
#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum FaultPoint {
    RawInitialization,
    Initialization,
    Staging,
    Index,
    CommitObject,
    Reference,
    FileFsync,
    DirectoryFsync,
    Reopen,
    Restoration,
    FetchedReadback,
    RecoveryReference,
    Readback,
}
#[cfg(all(test, unix))]
type AfterEofHook = ((u64, u64), Box<dyn FnOnce()>);
#[cfg(all(test, unix))]
thread_local! {
    static FAILURE:std::cell::Cell<Option<FaultPoint>>=const {std::cell::Cell::new(None)};
    static AFTER_EOF:std::cell::RefCell<Option<AfterEofHook>>=const {std::cell::RefCell::new(None)};
}
#[cfg(unix)]
fn fault(point: FaultPoint) -> Result<()> {
    #[cfg(test)]
    if FAILURE.with(|failure| {
        if failure.get() == Some(point) {
            failure.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(C10RepositoryErrorV1::Io(io::Error::other(
            "synthetic C10 fault",
        )));
    }
    let _ = point;
    Ok(())
}
#[cfg(not(unix))]
impl C10PayloadSnapshotV1 {
    pub(crate) fn capture(_project: &Path, _manifest: &ShareManifest) -> Result<Self> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        Err(C10RepositoryErrorV1::Unavailable)
    }
}

#[cfg(not(unix))]
impl C10LocalRepositoryV1 {
    pub(crate) fn inspect(
        _state: &Path,
        _git: &ExecutableIdentityV1,
    ) -> Result<C10RepositoryInspectionV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn revalidate_controls(&self) -> Result<()> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn run_fixture_child(&self, _args: &[OsString]) -> Result<GitChildOutputV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn run_network_child(
        &self,
        _owner: &GitCredentialOwnerV1,
        _material: &GitCredentialMaterialV1,
        _args: &[OsString],
    ) -> Result<GitChildOutputV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn require_ancestor(
        &self,
        _expected: &SyncGitObjectIdV1,
        _intended: &SyncGitObjectIdV1,
    ) -> Result<()> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn head_for_recovery(&self) -> Result<SyncPriorHeadV2> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn snapshot_clean(&self) -> Result<C10RepositorySnapshotV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn commit_after_local_prepared(
        self,
        _management: &crate::management_effect::ManagementEffectStoreV1,
        _started: &StartedManagementEffectV1,
        _lease: &ProjectMutationLeaseV1,
        _payload: &C10PayloadSnapshotV1,
    ) -> Result<(Self, C10CommitReadbackV1)> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn durable_current_after_unknown(
        self,
        _project: &Path,
        _management: &crate::management_effect::ManagementEffectStoreV1,
        _unknown: &UnknownManagementEffectV1,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<(Self, C10CommitReadbackV1)> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn repair_fetched_after_unknown(
        self,
        _project: &Path,
        _management: &crate::management_effect::ManagementEffectStoreV1,
        _unknown: &UnknownManagementEffectV1,
        _lease: &ProjectMutationLeaseV1,
        _expected_current: &SyncPriorHeadV2,
        _fetched: C10ExactRecoveryFetchV1,
    ) -> Result<(Self, C10CommitReadbackV1)> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn restore_after_unknown(
        self,
        _project: &Path,
        _management: &crate::management_effect::ManagementEffectStoreV1,
        _unknown: &UnknownManagementEffectV1,
        _lease: &ProjectMutationLeaseV1,
        _expected_current: &SyncPriorHeadV2,
    ) -> Result<(Self, C10RestoreReadbackV1)> {
        Err(C10RepositoryErrorV1::Unavailable)
    }
}

#[cfg(not(unix))]
impl C10AbsentRepositoryV1 {
    pub(crate) fn snapshot_clean(&self) -> Result<C10RepositorySnapshotV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn observe_exact_child(
        &self,
        _owner: &GitCredentialOwnerV1,
        _binding: &GitCredentialBindingV1,
        _remote: &CanonicalHttpsRemoteV1,
    ) -> Result<GitChildOutputV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    #[cfg(test)]
    pub(crate) fn observe_exact_fixture_child(
        &self,
        _remote: &CanonicalHttpsRemoteV1,
    ) -> Result<GitChildOutputV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn restore_absent_after_unknown(
        self,
        _project: &Path,
        _management: &crate::management_effect::ManagementEffectStoreV1,
        _unknown: &UnknownManagementEffectV1,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<C10RestoreReadbackV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }

    pub(crate) fn initialize_after_local_prepared(
        self,
        _management: &crate::management_effect::ManagementEffectStoreV1,
        _started: &StartedManagementEffectV1,
        _lease: &ProjectMutationLeaseV1,
        _payload: &C10PayloadSnapshotV1,
    ) -> Result<C10LocalRepositoryV1> {
        Err(C10RepositoryErrorV1::Unavailable)
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn c10_repository_config_allows_only_typed_harmless_core_keys() {
        let good=b"[core]\nrepositoryformatversion=0\nfilemode=true\nbare=false\nlogallrefupdates=true\nignorecase=false\nprecomposeunicode=true\n";
        assert_eq!(parse_config(good).unwrap().len(), 6);
        for bad in [
            "[include]\npath=/tmp/other",
            "[credential]\nhelper=bad",
            "[core]\nsshCommand=bad",
            "[core]\nfsmonitor=bad",
            "[core]\nrepositoryformatversion=1",
            "[core]\nrepositoryformatversion=0\nfilemode=true\nbare=true\nlogallrefupdates=true",
            "[core]\nrepositoryformatversion=0\nfilemode=true\nbare=false\nlogallrefupdates=true\nunknown=true",
        ] {
            assert!(parse_config(bad.as_bytes()).is_err());
        }
    }

    #[test]
    fn c10_repository_only_exact_operation_recovery_ref_names_are_known() {
        assert!(control_kind("refs", "polaris", true).is_ok());
        assert!(control_kind("refs/polaris", "recovery", true).is_ok());
        assert!(control_kind("refs/polaris/recovery", &"a".repeat(64), false).is_ok());
        let uppercase = "A".repeat(64);
        for unknown in ["a", "unknown", uppercase.as_str()] {
            assert!(control_kind("refs/polaris/recovery", unknown, false).is_err());
        }
        assert!(control_kind("refs/heads", "foreign", false).is_err());
        assert!(!known_ref("refs/foreign/unknown"));
        assert!(known_ref(&format!(
            "refs/polaris/recovery/{}",
            "a".repeat(64)
        )));
        assert!(control_kind("logs", "refs", true).is_ok());
        assert!(control_kind("logs", "HEAD", false).is_ok());
        assert!(control_kind("logs/refs", "heads", true).is_ok());
        assert!(control_kind("logs/refs/heads", "polaris", false).is_ok());
        assert!(control_kind("logs/refs/polaris/recovery", &"a".repeat(64), false).is_ok());
        assert!(control_kind("logs", "heads", true).is_err());
        assert!(control_kind("logs/refs/heads", "foreign", false).is_err());
        assert!(control_kind("logs/refs/tags", "tag", false).is_err());
        assert!(control_kind("logs/refs/polaris/recovery", "unknown", false).is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests;
