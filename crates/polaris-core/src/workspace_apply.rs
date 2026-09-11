//! macOS file-only apply-back with retained originals and a durable intent journal.
//! The caller stops snapshot writers and serializes secret registration for the
//! entire call (including candidate staging and capture validation).
//! Recovery directories are deliberately never garbage-collected. This is not a
//! multi-file transaction or a CAS against non-cooperating editors/open file FDs.

#![cfg(target_os = "macos")]

use crate::isolated_workspace::{
    self as copy, Change, ChangeSet, Limits, ManifestEntry, RegisteredSecrets, Snapshot,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata, Permissions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Phase {
    Preparing,
    Prepared,
    PreserveIntent,
    Preserved,
    RestoreIntent,
    Restored,
    InstallIntent,
    Installed,
    DeleteIntent,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum FailureKind {
    ProtectionUnavailable,
    InvalidChangeSet,
    UnsupportedEntry,
    Conflict,
    Secret,
    AncestorChanged,
    CrossDevice,
    Io,
    RestoreConflict,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyFailure {
    pub kind: FailureKind,
    pub entry: Option<usize>,
    pub os_error: Option<i32>,
}

/// Flags describe completed syscalls even if their subsequent sync/journal failed.
#[derive(Clone, Debug, Serialize)]
pub struct EntryReport {
    pub relative_path: PathBuf,
    /// Includes ancestors above source; a path may no longer name this directory.
    pub source_parent_chain: Vec<(PathBuf, (u64, u64))>,
    pub phase: Phase,
    pub old_retained: bool,
    pub installed: bool,
    pub deleted: bool,
    pub restored: bool,
    pub old_name: String,
    /// A separate fixed copy; installing never consumes this recovery version.
    pub candidate_name: Option<String>,
    pub install_name: Option<String>,
    pub candidate_identity: Option<(u64, u64)>,
    pub install_identity: Option<(u64, u64)>,
}

#[derive(Debug, Serialize)]
pub struct ApplyReport {
    pub source_path: PathBuf,
    /// Intended path, not a guarantee that an external rename left it attached.
    pub recovery_path: Option<PathBuf>,
    pub recovery_identity: Option<(u64, u64)>,
    #[serde(skip)]
    recovery_directory: Option<File>,
    pub entries: Vec<EntryReport>,
    pub failure: Option<ApplyFailure>,
}

impl ApplyReport {
    /// Remains usable if an ancestor of the recovery path was renamed. The
    /// caller may retain this handle while presenting/manual recovery proceeds.
    pub fn recovery_directory(&self) -> Option<&File> {
        self.recovery_directory.as_ref()
    }
}

type Result<T> = std::result::Result<T, ApplyFailure>;
fn fail(kind: FailureKind) -> ApplyFailure {
    ApplyFailure {
        kind,
        entry: None,
        os_error: None,
    }
}
fn io(error: std::io::Error) -> ApplyFailure {
    ApplyFailure {
        kind: if error.raw_os_error() == Some(libc::EXDEV) {
            FailureKind::CrossDevice
        } else {
            FailureKind::Io
        },
        entry: None,
        os_error: error.raw_os_error(),
    }
}
fn cstr(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| fail(FailureKind::InvalidChangeSet))
}
fn identity(m: &Metadata) -> (u64, u64) {
    (m.dev(), m.ino())
}
fn stable(a: &Metadata, b: &Metadata) -> bool {
    identity(a) == identity(b)
        && a.len() == b.len()
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn open_at(parent: &File, name: &OsStr, directory: bool, create: bool) -> Result<File> {
    let name = cstr(name)?;
    let flags = libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        }
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: live dirfd, single validated name, NUL-terminated argument.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Trusted operation-specific recovery parent. The path is provenance only;
/// the owned descriptor, bound to the approved identity, selects the directory.
#[derive(Debug)]
pub struct RecoveryParent {
    directory: File,
    expected: (u64, u64),
    provenance: PathBuf,
}
impl RecoveryParent {
    /// The caller keeps this directory outside child permissions and binds its
    /// identity in the durable intent before dispatch. No auth lock is acquired.
    pub fn pin(
        directory: &File,
        expected: (u64, u64),
        provenance: &Path,
    ) -> std::result::Result<Self, ApplyFailure> {
        let pinned = Self {
            directory: directory.try_clone().map_err(io)?,
            expected,
            provenance: provenance.into(),
        };
        pinned.validate_identity()?;
        Ok(pinned)
    }
    /// Recheck the held FD's identity, directory type, ownership and private mode
    /// immediately before committing intent. Never reopens the provenance path,
    /// writes, or grants authorization. Apply also checks filesystem and scope.
    pub fn validate_identity(&self) -> std::result::Result<(), ApplyFailure> {
        private_directory(&self.directory, self.expected)
    }

    /// The expected (device, inode) bound at pin time; not a fresh validation.
    pub fn identity(&self) -> (u64, u64) {
        self.expected
    }
    /// Original informational path, shared by approval and report evidence.
    /// It may have been renamed; only the held FD selects the recovery directory.
    pub fn provenance(&self) -> &Path {
        &self.provenance
    }

    /// Metadata-only pre-intent validation of the pinned recovery scope. Apply
    /// repeats its checks; this does not grant write permission or lock paths.
    pub fn validate_for_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        self.validate_identity()?;
        let source = Anchor::open(&snapshot.source_path)?;
        let metadata = source.dir.metadata().map_err(io)?;
        if identity(&metadata)
            != (
                snapshot.source_identity.device,
                snapshot.source_identity.inode,
            )
        {
            return Err(fail(FailureKind::AncestorChanged));
        }
        if metadata.dev() != self.expected.0 {
            return Err(fail(FailureKind::CrossDevice));
        }
        let copy_parent = Anchor::open(
            snapshot
                .path()
                .parent()
                .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?,
        )?;
        outside(&self.directory, &source.dir)?;
        outside(&self.directory, &copy_parent.dir)?;
        source.check()?;
        copy_parent.check()
    }
}

fn private_directory(directory: &File, expected: (u64, u64)) -> Result<()> {
    let metadata = directory.metadata().map_err(io)?;
    if identity(&metadata) != expected {
        return Err(fail(FailureKind::AncestorChanged));
    }
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7077 != 0
        || metadata.mode() & 0o700 != 0o700
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    Ok(())
}

/// Compare actual ancestor identities, never the informational recovery path.
fn outside(directory: &File, forbidden: &File) -> Result<()> {
    let forbidden = identity(&forbidden.metadata().map_err(io)?);
    let mut current = directory.try_clone().map_err(io)?;
    for _ in 0..1024 {
        let here = identity(&current.metadata().map_err(io)?);
        if here == forbidden {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        let parent = open_at(&current, OsStr::new(".."), true, false)?;
        if identity(&parent.metadata().map_err(io)?) == here {
            return Ok(());
        }
        current = parent;
    }
    Err(fail(FailureKind::InvalidChangeSet))
}

struct Anchor {
    path: PathBuf,
    dir: File,
    chain: Vec<(PathBuf, (u64, u64))>,
    pinned: Option<(u64, u64)>,
}
impl Anchor {
    fn open(path: &Path) -> Result<Self> {
        if path.to_str().is_none()
            || !path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        let mut dir = File::open("/").map_err(io)?;
        let mut prefix = PathBuf::from("/");
        let mut chain = vec![(prefix.clone(), identity(&dir.metadata().map_err(io)?))];
        for component in path.components() {
            if let Component::Normal(name) = component {
                dir = open_at(&dir, name, true, false)?;
                prefix.push(name);
                chain.push((prefix.clone(), identity(&dir.metadata().map_err(io)?)));
            }
        }
        Ok(Self {
            path: path.into(),
            dir,
            chain,
            pinned: None,
        })
    }
    fn check(&self) -> Result<()> {
        if let Some(expected) = self.pinned {
            return private_directory(&self.dir, expected);
        }
        let now = Self::open(&self.path).map_err(|_| fail(FailureKind::AncestorChanged))?;
        if self.chain != now.chain {
            return Err(fail(FailureKind::AncestorChanged));
        }
        Ok(())
    }
}

fn exclusive(from: &File, name: &OsStr, to: &File, target: &OsStr) -> Result<()> {
    let name = cstr(name)?;
    let target = cstr(target)?;
    // EXCL tests destination absence; source identity is checked after capture.
    let status = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if status != 0 {
        let mut error = io(std::io::Error::last_os_error());
        if matches!(error.os_error, Some(libc::EEXIST | libc::ENOENT)) {
            error.kind = FailureKind::Conflict;
        }
        return Err(error);
    }
    Ok(())
}

fn matching(a: &ManifestEntry, b: &ManifestEntry) -> bool {
    a.relative_path == b.relative_path
        && a.identity == b.identity
        && a.bytes == b.bytes
        && a.mode == b.mode
        && a.sha256 == b.sha256
        && a.observed_mode() == b.observed_mode()
}
fn same_change(a: &Change, b: &Change) -> bool {
    let same = |a: &Option<ManifestEntry>, b: &Option<ManifestEntry>| match (a, b) {
        (Some(a), Some(b)) => matching(a, b),
        (None, None) => true,
        _ => false,
    };
    a.relative_path == b.relative_path
        && a.kind == b.kind
        && same(&a.before, &b.before)
        && same(&a.after, &b.after)
}

fn validate_changes(
    snapshot: &Snapshot,
    requested: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<ChangeSet> {
    // The requested public Vec and entries are not authoritative. A full fresh
    // change scan also detects omitted, duplicate, reordered and edited entries.
    let fresh = copy::collect_changes(snapshot, secrets, limits)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if !requested.is_applicable()
        || !fresh.is_applicable()
        || requested.changes.len() != fresh.changes.len()
        || !requested
            .changes
            .iter()
            .zip(&fresh.changes)
            .all(|(a, b)| same_change(a, b))
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    // Checks the request's private snapshot binding as well as the real source.
    let conflicts = copy::check_source_conflicts(snapshot, requested, secrets, limits)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if !conflicts.is_empty() {
        return Err(fail(FailureKind::Conflict));
    }
    for change in &fresh.changes {
        if change.relative_path.to_str().is_none()
            || change
                .before
                .iter()
                .chain(change.after.iter())
                .any(|entry| entry.sha256.is_none())
        {
            return Err(fail(FailureKind::UnsupportedEntry));
        }
    }
    Ok(fresh)
}

/// Verify identity/registration before reading bytes, including a moved source.
fn verified_copy(
    file: &mut File,
    destination: Option<&mut File>,
    expected: &ManifestEntry,
    original_path: &Path,
    physical_path: &Path,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<()> {
    let before = file.metadata().map_err(io)?;
    let secrets = copy::refreshed_secrets(secrets).map_err(|_| fail(FailureKind::Secret))?;
    if polaris_tools::path_policy::is_denied(original_path)
        || polaris_tools::path_policy::is_denied(physical_path)
        || secrets.denies(original_path, &before)
        || secrets.denies(physical_path, &before)
    {
        return Err(fail(FailureKind::Secret));
    }
    if !before.is_file() || before.nlink() != 1 {
        return Err(fail(FailureKind::UnsupportedEntry));
    }
    if identity(&before) != (expected.identity.device, expected.identity.inode)
        || before.len() != expected.bytes
        || before.mode() & 0o7777 != expected.observed_mode()
        || before.len() > limits.max_file_bytes
    {
        return Err(fail(FailureKind::Conflict));
    }
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    let mut destination = destination;
    loop {
        let n = file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or_else(|| fail(FailureKind::Conflict))?;
        if total > expected.bytes {
            return Err(fail(FailureKind::Conflict));
        }
        hash.update(&buffer[..n]);
        if let Some(output) = &mut destination {
            output.write_all(&buffer[..n]).map_err(io)?;
        }
    }
    if total != expected.bytes
        || Some(<[u8; 32]>::from(hash.finalize())) != expected.sha256
        || !stable(&before, &file.metadata().map_err(io)?)
    {
        return Err(fail(FailureKind::Conflict));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    BeforePrepare,
    AfterPrepared,
    BeforePreserve,
    AfterPreserve,
    AfterVerified,
    BeforeInstall,
    AfterInstall,
    BeforeRestore,
    AfterRestore,
    BeforeDelete,
    AfterDelete,
    BeforeJournal,
    AfterJournal,
}
type Hook<'a> = dyn FnMut(Point, usize, &Path) -> std::io::Result<()> + 'a;
fn checkpoint(hook: &mut Hook<'_>, point: Point, index: usize, recovery: &Path) -> Result<()> {
    #[cfg(test)]
    tests::panic_at_checkpoint(point);
    hook(point, index, recovery).map_err(io)
}

fn journal(
    file: &mut File,
    entry: &EntryReport,
    index: usize,
    recovery: &Path,
    hook: &mut Hook<'_>,
) -> Result<()> {
    checkpoint(hook, Point::BeforeJournal, index, recovery)?;
    serde_json::to_writer(&mut *file, entry).map_err(|_| fail(FailureKind::Io))?;
    file.write_all(b"\n").map_err(io)?;
    file.sync_all().map_err(io)?;
    checkpoint(hook, Point::AfterJournal, index, recovery)
}

/// `recovery_parent` must be parent-controlled, not exposed to any child, and on
/// the source filesystem. It must outlive snapshots; never place it in scratch.
/// Permission/generation authorization and recovery UI belong to the caller.
pub fn apply(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
) -> ApplyReport {
    apply_with_hook(
        snapshot,
        changes,
        secrets,
        limits,
        recovery_parent,
        &mut |_, _, _| Ok(()),
    )
}

/// Apply while the authentication owner's registry/rotation lock is held for
/// the entire operation, including candidate staging and source capture.
/// The caller must already have stopped snapshot writers and authorized the
/// current run/generation. This wrapper grants no execution or write authority.
/// Recovery location requirements are the same as `apply`. Do not call while
/// already holding the auth lock or from an auth registry callback.
pub fn protected_apply(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    limits: Limits,
    recovery_parent: &Path,
) -> ApplyReport {
    let protected = polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered).map_err(|_| ())?;
        Ok::<_, ()>(apply(snapshot, changes, &secrets, limits, recovery_parent))
    });
    match protected {
        Ok(Ok(report)) => report,
        _ => ApplyReport {
            source_path: snapshot.source_path.clone(),
            recovery_path: None,
            recovery_identity: None,
            recovery_directory: None,
            entries: vec![],
            failure: Some(fail(FailureKind::ProtectionUnavailable)),
        },
    }
}

/// Trusted FD-bound counterpart of `protected_apply`. The caller must stop copy
/// writers and durably authorize this exact parent identity before dispatch.
/// Do not call while holding the authentication registry lock.
pub fn protected_apply_pinned(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    limits: Limits,
    recovery_parent: &RecoveryParent,
) -> ApplyReport {
    let protected = polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered).map_err(|_| ())?;
        Ok::<_, ()>(apply_pinned_with_hook(
            snapshot,
            changes,
            &secrets,
            limits,
            recovery_parent,
            &mut |_, _, _| Ok(()),
        ))
    });
    protected
        .ok()
        .and_then(|report| report.ok())
        .unwrap_or_else(|| ApplyReport {
            source_path: snapshot.source_path.clone(),
            recovery_path: None,
            recovery_identity: None,
            recovery_directory: None,
            entries: vec![],
            failure: Some(fail(FailureKind::ProtectionUnavailable)),
        })
}

fn apply_pinned_with_hook(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &RecoveryParent,
    hook: &mut Hook<'_>,
) -> ApplyReport {
    apply_with_parent(
        snapshot,
        changes,
        secrets,
        limits,
        &recovery_parent.provenance,
        Some(recovery_parent),
        hook,
    )
}

fn apply_with_hook(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
    hook: &mut Hook<'_>,
) -> ApplyReport {
    apply_with_parent(
        snapshot,
        changes,
        secrets,
        limits,
        recovery_parent,
        None,
        hook,
    )
}

fn apply_with_parent(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
    pinned: Option<&RecoveryParent>,
    hook: &mut Hook<'_>,
) -> ApplyReport {
    let mut report = ApplyReport {
        source_path: snapshot.source_path.clone(),
        recovery_path: None,
        recovery_identity: None,
        recovery_directory: None,
        entries: vec![],
        failure: None,
    };
    let outcome = run(
        snapshot,
        changes,
        secrets,
        limits,
        recovery_parent,
        pinned,
        hook,
        &mut report,
    );
    if let Err(error) = outcome {
        report.failure = Some(error);
    }
    report
}

#[allow(clippy::too_many_arguments)]
fn run(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
    pinned: Option<&RecoveryParent>,
    hook: &mut Hook<'_>,
    report: &mut ApplyReport,
) -> Result<()> {
    let source = Anchor::open(&snapshot.source_path)?;
    let owner = if let Some(pinned) = pinned {
        pinned.validate_identity()?;
        Anchor {
            path: pinned.provenance.clone(),
            dir: pinned.directory.try_clone().map_err(io)?,
            chain: vec![(pinned.provenance.clone(), pinned.expected)],
            pinned: Some(pinned.expected),
        }
    } else {
        Anchor::open(recovery_parent)?
    };
    if source.dir.metadata().map_err(io)?.dev() != owner.dir.metadata().map_err(io)?.dev() {
        return Err(fail(FailureKind::CrossDevice));
    }
    let snapshot_owner = Anchor::open(
        snapshot
            .path()
            .parent()
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?,
    )?;
    outside(&owner.dir, &source.dir)?;
    outside(&owner.dir, &snapshot_owner.dir)?;
    let fresh = validate_changes(snapshot, changes, secrets, limits)?;
    if fresh.changes.is_empty() {
        return Ok(());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| fail(FailureKind::Io))?;
    let name = format!(
        "polaris-apply-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let recovery_path = recovery_parent.join(&name);
    owner.check()?;
    let c_name = cstr(OsStr::new(&name))?;
    if unsafe { libc::mkdirat(owner.dir.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
        return Err(io(std::io::Error::last_os_error()));
    }
    report.recovery_path = Some(recovery_path.clone());
    let directory = open_at(&owner.dir, OsStr::new(&name), true, false)?;
    let recovery_id = identity(&directory.metadata().map_err(io)?);
    let mut chain = owner.chain.clone();
    chain.push((recovery_path.clone(), recovery_id));
    let recovery = Anchor {
        path: recovery_path.clone(),
        dir: directory,
        chain,
        pinned: pinned.map(|_| recovery_id),
    };
    report.recovery_identity = Some(identity(&recovery.dir.metadata().map_err(io)?));
    report.recovery_directory = Some(recovery.dir.try_clone().map_err(io)?);
    owner.dir.sync_all().map_err(io)?;
    owner.check()?;
    recovery.check()?;
    let mut log = open_at(&recovery.dir, OsStr::new("journal.jsonl"), false, true)?;
    // Persist identities, baseline/target hashes and modes before any source move.
    let header = serde_json::json!({ "version": 1, "source": snapshot.source_path, "source_chain": source.chain,
        "recovery_chain": recovery.chain, "entries": fresh.changes.iter().map(|c| serde_json::json!({
            "path": c.relative_path, "before": c.before.as_ref().map(|e| serde_json::json!({"device": e.identity.device, "inode":e.identity.inode, "hash":e.sha256, "mode":e.observed_mode(), "bytes":e.bytes})),
            "after": c.after.as_ref().map(|e| serde_json::json!({"hash":e.sha256, "mode":e.mode, "bytes":e.bytes}))
        })).collect::<Vec<_>>() });
    serde_json::to_writer(&mut log, &header).map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    log.write_all(b"\n").map_err(io)?;
    log.sync_all().map_err(io)?;
    recovery.dir.sync_all().map_err(io)?;
    let mut parents = Vec::new();
    // Fix all candidates before opening a gap at any source entry.
    for (i, change) in fresh.changes.iter().enumerate() {
        let prepare = (|| {
            let parent_path = snapshot
                .source_path
                .join(&change.relative_path)
                .parent()
                .unwrap()
                .to_path_buf();
            let parent = Anchor::open(&parent_path)?;
            if parent.dir.metadata().map_err(io)?.dev()
                != recovery.dir.metadata().map_err(io)?.dev()
            {
                return Err(fail(FailureKind::CrossDevice));
            }
            let candidate_name = change.after.as_ref().map(|_| format!("candidate-{i}"));
            let install_name = change.after.as_ref().map(|_| format!("install-{i}"));
            report.entries.push(EntryReport {
                relative_path: change.relative_path.clone(),
                source_parent_chain: parent.chain.clone(),
                phase: Phase::Preparing,
                old_retained: false,
                installed: false,
                deleted: false,
                restored: false,
                old_name: format!("old-{i}"),
                candidate_name,
                install_name,
                candidate_identity: None,
                install_identity: None,
            });
            checkpoint(hook, Point::BeforePrepare, i, &recovery_path)?;
            if let Some(after) = &change.after {
                let copy_path = snapshot.path().join(&change.relative_path);
                let copy_parent = Anchor::open(copy_path.parent().unwrap())?;
                let mut input = open_at(
                    &copy_parent.dir,
                    copy_path.file_name().unwrap(),
                    false,
                    false,
                )?;
                let mut candidate = open_at(
                    &recovery.dir,
                    OsStr::new(report.entries[i].candidate_name.as_ref().unwrap()),
                    false,
                    true,
                )?;
                verified_copy(
                    &mut input,
                    Some(&mut candidate),
                    after,
                    &snapshot.source_path.join(&change.relative_path),
                    &copy_path,
                    secrets,
                    limits,
                )?;
                copy_parent.check()?;
                candidate.sync_all().map_err(io)?;
                report.entries[i].candidate_identity =
                    Some(identity(&candidate.metadata().map_err(io)?));
                candidate.seek(SeekFrom::Start(0)).map_err(io)?;
                let mut install = open_at(
                    &recovery.dir,
                    OsStr::new(report.entries[i].install_name.as_ref().unwrap()),
                    false,
                    true,
                )?;
                let copied = std::io::copy(&mut candidate, &mut install).map_err(io)?;
                if copied != after.bytes {
                    return Err(fail(FailureKind::Conflict));
                }
                install
                    .set_permissions(Permissions::from_mode(after.mode & 0o777))
                    .map_err(io)?;
                install.sync_all().map_err(io)?;
                report.entries[i].install_identity =
                    Some(identity(&install.metadata().map_err(io)?));
            }
            recovery.dir.sync_all().map_err(io)?;
            report.entries[i].phase = Phase::Prepared;
            journal(&mut log, &report.entries[i], i, &recovery_path, hook)?;
            checkpoint(hook, Point::AfterPrepared, i, &recovery_path)?;
            parents.push(parent);
            Ok(())
        })();
        prepare.map_err(|mut e: ApplyFailure| {
            e.entry = Some(i);
            e
        })?;
    }
    // Recheck after staging, before the first source mutation. Thereafter each
    // captured inode is validated independently; earlier successes are not undone.
    if !copy::check_source_conflicts(snapshot, &fresh, secrets, limits)
        .map_err(|_| fail(FailureKind::Conflict))?
        .is_empty()
    {
        return Err(fail(FailureKind::Conflict));
    }
    for (i, (change, parent)) in fresh.changes.iter().zip(&parents).enumerate() {
        let entry = &mut report.entries[i];
        let result = apply_one(
            change, parent, &recovery, &mut log, entry, i, snapshot, secrets, limits, hook,
        );
        result.map_err(|mut e| {
            e.entry = Some(i);
            e
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_one(
    change: &Change,
    parent: &Anchor,
    recovery: &Anchor,
    log: &mut File,
    entry: &mut EntryReport,
    i: usize,
    snapshot: &Snapshot,
    secrets: &RegisteredSecrets,
    limits: Limits,
    hook: &mut Hook<'_>,
) -> Result<()> {
    let name = change
        .relative_path
        .file_name()
        .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
    let check = || {
        parent.check()?;
        recovery.check()
    };
    check()?;
    if let Some(before) = &change.before {
        entry.phase = Phase::PreserveIntent;
        journal(log, entry, i, &recovery.path, hook)?;
        checkpoint(hook, Point::BeforePreserve, i, &recovery.path)?;
        check()?;
        exclusive(
            &parent.dir,
            name,
            &recovery.dir,
            OsStr::new(&entry.old_name),
        )?;
        entry.old_retained = true;
        checkpoint(hook, Point::AfterPreserve, i, &recovery.path)?;
        check()?;
        parent.dir.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        entry.phase = Phase::Preserved;
        journal(log, entry, i, &recovery.path, hook)?;
        let verification = (|| {
            let mut old = open_at(&recovery.dir, OsStr::new(&entry.old_name), false, false)?;
            verified_copy(
                &mut old,
                None,
                before,
                &snapshot.source_path.join(&change.relative_path),
                &recovery.path.join(&entry.old_name),
                secrets,
                limits,
            )?;
            old.sync_all().map_err(io)
        })();
        if let Err(original) = verification {
            restore_current(parent, recovery, log, entry, i, hook)?;
            return Err(original);
        }
        checkpoint(hook, Point::AfterVerified, i, &recovery.path)?;
    }
    if let Some(install) = &entry.install_name {
        entry.phase = Phase::InstallIntent;
        journal(log, entry, i, &recovery.path, hook)?;
        checkpoint(hook, Point::BeforeInstall, i, &recovery.path)?;
        check()?;
        if let Err(error) = exclusive(&recovery.dir, OsStr::new(install), &parent.dir, name) {
            // A known destination conflict can only restore into a now-empty
            // slot. Other I/O failures keep the intent unresolved for recovery.
            if error.os_error == Some(libc::EEXIST) && entry.old_retained {
                restore_current(parent, recovery, log, entry, i, hook)?;
            }
            return Err(error);
        }
        entry.installed = true;
        checkpoint(hook, Point::AfterInstall, i, &recovery.path)?;
        check()?;
        parent.dir.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        entry.phase = Phase::Installed;
    } else {
        entry.phase = Phase::DeleteIntent;
        journal(log, entry, i, &recovery.path, hook)?;
        checkpoint(hook, Point::BeforeDelete, i, &recovery.path)?;
        check()?;
        // Never unlink a writer's newly created entry while completing deletion.
        let c_name = cstr(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let status = unsafe {
            libc::fstatat(
                parent.dir.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status == 0 {
            return Err(fail(FailureKind::Conflict));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(io(error));
        }
        entry.deleted = true;
        checkpoint(hook, Point::AfterDelete, i, &recovery.path)?;
        check()?;
        parent.dir.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        entry.phase = Phase::Deleted;
    }
    journal(log, entry, i, &recovery.path, hook)
}

fn restore_current(
    parent: &Anchor,
    recovery: &Anchor,
    log: &mut File,
    entry: &mut EntryReport,
    i: usize,
    hook: &mut Hook<'_>,
) -> Result<()> {
    parent.check()?;
    recovery.check()?;
    entry.phase = Phase::RestoreIntent;
    journal(log, entry, i, &recovery.path, hook)?;
    checkpoint(hook, Point::BeforeRestore, i, &recovery.path)?;
    parent.check()?;
    recovery.check()?;
    if let Err(mut error) = exclusive(
        &recovery.dir,
        OsStr::new(&entry.old_name),
        &parent.dir,
        entry
            .relative_path
            .file_name()
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?,
    ) {
        error.kind = FailureKind::RestoreConflict;
        return Err(error);
    }
    entry.old_retained = false;
    entry.restored = true;
    checkpoint(hook, Point::AfterRestore, i, &recovery.path)?;
    parent.check()?;
    recovery.check()?;
    parent.dir.sync_all().map_err(io)?;
    recovery.dir.sync_all().map_err(io)?;
    entry.phase = Phase::Restored;
    journal(log, entry, i, &recovery.path, hook)
}

#[cfg(test)]
#[path = "workspace_apply/tests.rs"]
mod tests;
