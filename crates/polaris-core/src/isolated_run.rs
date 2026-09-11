//! macOS prepare-only composition. Paths and executable identity are supplied by
//! the trusted owner, never by model output. Load/register auth paths first.
//! Retain this owner until process cleanup completes; this type does not enforce
//! that lifecycle, launch a process, or apply changes back to the source.
#![cfg(target_os = "macos")]

use crate::isolated_workspace::{self, Limits, RegisteredSecrets, Snapshot};
use polaris_auth::protection::ProtectedPathsSnapshot;
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use std::{
    ffi::CString,
    fs::{self, File, Metadata, Permissions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

/// Explicit bound, independent of source-workspace copy limits.
pub const MAX_HELPER_BYTES: u64 = 128 * 1024 * 1024;
const SYSTEM_ROOTS: &[&str] = &["/bin", "/usr/bin", "/usr/lib", "/System/Library"];

pub mod toolchains;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    ProtectionUnavailable,
    InvalidPath,
    UnsafeHelper,
    RuntimeDenied,
    Changed,
    Limit,
    Io,
    Snapshot,
    Policy,
}
impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "隔離実行の準備を拒否しました: {self:?}")
    }
}
impl std::error::Error for PrepareError {}
type Result<T> = std::result::Result<T, PrepareError>;
fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|_| PrepareError::Io)
}

/// Trusted preparation-only read of a pinned helper. The callback runs under
/// the auth registry lock and must not reenter protected preparation APIs.
/// Path checks alone are insufficient: a previously registered secret inode
/// remains forbidden even after it has been moved to a different path.
pub fn with_protected_helper_read<T>(
    path: &Path,
    pinned: &File,
    read: impl FnOnce(&File) -> T,
) -> Result<T> {
    polaris_auth::protection::with_protected_paths(|protected| {
        physical(path)?;
        let current = open_physical(path, false)?;
        let current_metadata = io(current.metadata())?;
        let metadata = io(pinned.metadata())?;
        helper_metadata(&metadata)?;
        if current_metadata.dev() != metadata.dev() || current_metadata.ino() != metadata.ino() {
            return Err(PrepareError::Changed);
        }
        let secrets = RegisteredSecrets::from_auth_registry(protected)
            .map_err(|_| PrepareError::ProtectionUnavailable)?;
        if secrets.denies(path, &metadata) || polaris_tools::path_policy::is_denied(path) {
            return Err(PrepareError::UnsafeHelper);
        }
        Ok(read(pinned))
    })
    .map_err(|_| PrepareError::ProtectionUnavailable)?
}

pub struct PreparedWorkspace {
    snapshot: Snapshot,
    policy: SandboxPolicy,
    helper: PathBuf,
    // Restore directory owner-write permission before Snapshot's TempDir cleanup.
    // Kept as an fd so cleanup does not chmod a rebound path.
    runtime: File,
    // Separate copies are never part of the source change/apply manifest.
    _toolchains: toolchains::StagedToolchains,
}
impl PreparedWorkspace {
    pub fn prepare(
        source: &Path,
        trusted_executable: &Path,
        mode: SandboxMode,
        runtime_roots: &[PathBuf],
        limits: Limits,
    ) -> Result<Self> {
        polaris_auth::protection::with_protected_paths(|protected| {
            prepare_locked(
                source,
                trusted_executable,
                mode,
                runtime_roots,
                limits,
                protected,
            )
        })
        .map_err(|_| PrepareError::ProtectionUnavailable)?
    }
    /// Prepare explicitly selected relocatable packages, without exposing their
    /// original roots. This does not resolve dependencies or relocate binaries.
    pub fn prepare_with_toolchains(
        source: &Path,
        trusted_executable: &Path,
        mode: SandboxMode,
        runtime_roots: &[PathBuf],
        limits: Limits,
        plan: &toolchains::ToolchainPlan,
    ) -> Result<Self> {
        polaris_auth::protection::with_protected_paths(|protected| {
            prepare_with_toolchains_locked(
                source,
                trusted_executable,
                mode,
                runtime_roots,
                limits,
                protected,
                Some(plan),
            )
        })
        .map_err(|_| PrepareError::ProtectionUnavailable)?
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }
    pub fn helper_path(&self) -> &Path {
        &self.helper
    }
}
impl Drop for PreparedWorkspace {
    fn drop(&mut self) {
        let _ = self.runtime.set_permissions(Permissions::from_mode(0o700));
    }
}

fn prepare_locked(
    source: &Path,
    executable: &Path,
    mode: SandboxMode,
    roots: &[PathBuf],
    limits: Limits,
    protected: &ProtectedPathsSnapshot,
) -> Result<PreparedWorkspace> {
    prepare_with_toolchains_locked(source, executable, mode, roots, limits, protected, None)
}

fn prepare_with_toolchains_locked(
    source: &Path,
    executable: &Path,
    mode: SandboxMode,
    roots: &[PathBuf],
    limits: Limits,
    protected: &ProtectedPathsSnapshot,
    plan: Option<&toolchains::ToolchainPlan>,
) -> Result<PreparedWorkspace> {
    physical(source)?;
    physical(executable)?;
    if executable.starts_with(source)
        || ancestor_has_identity(
            executable.parent().ok_or(PrepareError::InvalidPath)?,
            &io(open_physical(source, true)?.metadata())?,
        )?
    {
        return Err(PrepareError::UnsafeHelper);
    }
    let secrets = RegisteredSecrets::from_auth_registry(protected)
        .map_err(|_| PrepareError::ProtectionUnavailable)?;
    let mut input = open_physical(executable, false)?;
    let before = io(input.metadata())?;
    helper_metadata(&before)?;
    if secrets.denies(executable, &before) || polaris_tools::path_policy::is_denied(executable) {
        return Err(PrepareError::UnsafeHelper);
    }
    validate_runtime_roots(roots, source, protected.paths(), &secrets)?;
    // Same algorithm as protected_snapshot, but do not reenter its auth mutex.
    // The lock covers both workspace and executable copying, not just registration.
    let snapshot = isolated_workspace::snapshot(source, &secrets, limits)
        .map_err(|_| PrepareError::Snapshot)?;
    let root = snapshot.path().parent().ok_or(PrepareError::InvalidPath)?;
    for readable in roots {
        if overlaps(readable, root) {
            return Err(PrepareError::RuntimeDenied);
        }
    }
    let parent = open_physical(root, true)?;
    let meta = io(parent.metadata())?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(PrepareError::InvalidPath);
    }
    // Fixed sibling name, created relative to the privately owned root.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c"runtime".as_ptr(), 0o700) } != 0 {
        return Err(PrepareError::Io);
    }
    let runtime_path = root.join("runtime");
    let runtime = open_at(&parent, c"runtime", true, false)?;
    let helper = runtime_path.join("polaris");
    let mut output = open_at(&runtime, c"polaris", false, true)?;
    let copied = io(std::io::copy(
        &mut (&mut input).take(MAX_HELPER_BYTES + 1),
        &mut output,
    ))?;
    if copied > MAX_HELPER_BYTES || copied != before.len() {
        return Err(PrepareError::Limit);
    }
    io(output.flush())?;
    io(output.sync_all())?;
    if !stable(&before, &io(input.metadata())?)
        || !stable(&before, &io(open_physical(executable, false)?.metadata())?)
    {
        return Err(PrepareError::Changed);
    }
    // No executable write or setid permission survives staging.
    io(output.set_permissions(Permissions::from_mode(0o500)))?;
    let toolchains = toolchains::stage(plan, source, protected, &secrets)?;
    let mut readable = roots.to_vec();
    readable.push(runtime_path);
    readable.extend(toolchains.roots());
    let policy = SandboxPolicy::isolated(mode, snapshot.path(), &readable)
        .and_then(|p| {
            p.with_isolated_sibling_environment(snapshot.scratch_home(), snapshot.scratch_tmp())
        })
        .and_then(|p| p.with_isolated_runtime_bins(&toolchains.bin_dirs))
        .map_err(|_| PrepareError::Policy)?;
    // Verify the named private root still resolves to the pinned owner.
    if !identity(&meta, &io(open_physical(root, true)?.metadata())?) {
        return Err(PrepareError::Changed);
    }
    let prepared = PreparedWorkspace {
        snapshot,
        policy,
        helper,
        runtime,
        _toolchains: toolchains,
    };
    io(prepared
        .runtime
        .set_permissions(Permissions::from_mode(0o500)))?;
    Ok(prepared)
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn validate_runtime_roots(
    roots: &[PathBuf],
    source: &Path,
    protected: &std::collections::BTreeSet<PathBuf>,
    secrets: &RegisteredSecrets,
) -> Result<()> {
    if roots.len() > SYSTEM_ROOTS.len() {
        return Err(PrepareError::RuntimeDenied);
    }
    for root in roots {
        // Exact approved OS roots only. No arbitrary SDK/toolchain/user-cache trees.
        if !SYSTEM_ROOTS
            .iter()
            .any(|allowed| root.as_os_str() == Path::new(allowed).as_os_str())
        {
            return Err(PrepareError::RuntimeDenied);
        }
        physical(root)?;
        let metadata = io(open_physical(root, true)?.metadata())?;
        if metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || overlaps(root, source)
            || ancestor_has_identity(source, &metadata)?
            || ancestor_has_identity(root, &io(open_physical(source, true)?.metadata())?)?
            || secrets.denies(root, &metadata)
            || protected.iter().any(|secret| overlaps(root, secret))
        {
            return Err(PrepareError::RuntimeDenied);
        }
    }
    Ok(())
}
fn ancestor_has_identity(path: &Path, target: &Metadata) -> Result<bool> {
    for ancestor in path.ancestors() {
        if identity(target, &io(open_physical(ancestor, true)?.metadata())?) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn helper_metadata(metadata: &Metadata) -> Result<()> {
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o7022 != 0
        || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid())
    {
        return Err(PrepareError::UnsafeHelper);
    }
    if metadata.len() == 0 || metadata.len() > MAX_HELPER_BYTES {
        return Err(PrepareError::Limit);
    }
    Ok(())
}
fn physical(path: &Path) -> Result<()> {
    if path.as_os_str().len() > 4096
        || path.components().count() > 64
        || !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        || io(fs::canonicalize(path))?.as_os_str() != path.as_os_str()
    {
        return Err(PrepareError::InvalidPath);
    }
    Ok(())
}
fn open_physical(path: &Path, directory: bool) -> Result<File> {
    physical(path)?;
    let mut parent = io(File::open("/"))?;
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| {
            if let Component::Normal(name) = c {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    for (index, name) in parts.iter().enumerate() {
        let name = CString::new(name.as_bytes()).map_err(|_| PrepareError::InvalidPath)?;
        parent = open_at(&parent, &name, index + 1 < parts.len() || directory, false)?;
    }
    Ok(parent)
}
fn open_at(parent: &File, name: &std::ffi::CStr, directory: bool, create: bool) -> Result<File> {
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 }
        | if create {
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        };
    // SAFETY: live parent descriptor, fixed/validated NUL-terminated component;
    // successful fd ownership transfers exactly once into File.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(PrepareError::Io);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn identity(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn stable(a: &Metadata, b: &Metadata) -> bool {
    identity(a, b)
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
#[cfg(test)]
#[path = "isolated_run/tests.rs"]
mod tests;
