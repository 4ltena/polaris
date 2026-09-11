//! Reusable preparation recipe. Runs only on the owned preparation thread.
use super::{ConfirmedSourceGrant, FactoryError};
use polaris_core::{
    desktop_execution::SandboxMode,
    isolated_run::{MAX_HELPER_BYTES, PreparedWorkspace, toolchains::ToolchainPlan},
    isolated_workspace::Limits,
};
use std::{
    fs::{File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// Caller-supplied assembly manifest pins the actual helper bytes. Cloning a
/// recipe shares the verified helper FD, never a prepared mutable workspace.
#[derive(Clone)]
pub struct WorkspacePreparationRecipe {
    pub(super) source: PathBuf,
    helper: PathBuf,
    pinned_helper: Arc<File>,
    helper_metadata: Metadata,
    sha256: [u8; 32],
    limits: Limits,
    runtime_roots: Vec<PathBuf>,
    toolchains: Option<ToolchainPlan>,
    pub(super) copy_mutations: bool,
}
impl WorkspacePreparationRecipe {
    pub fn new(
        source: PathBuf,
        fixed_helper: PathBuf,
        expected_sha256: [u8; 32],
        limits: Limits,
        runtime_roots: Vec<PathBuf>,
        toolchains: Option<ToolchainPlan>,
        copy_mutations: bool,
    ) -> Result<Self, FactoryError> {
        if runtime_roots.len() > 32
            || limits.max_entries == 0
            || limits.max_files == 0
            || limits.max_file_bytes == 0
            || limits.max_total_bytes == 0
            || limits.max_depth == 0
        {
            return Err(FactoryError::Bounds);
        }
        if let Some(plan) = &toolchains
            && (plan.packages.len() > 32 || plan.packages.iter().any(|p| p.bin_dirs.len() > 32))
        {
            return Err(FactoryError::Bounds);
        }
        let file = open_path(&fixed_helper, false).map_err(|_| FactoryError::Workspace)?;
        let metadata = file.metadata().map_err(|_| FactoryError::Workspace)?;
        verify_bytes(&fixed_helper, &file, &metadata, &expected_sha256)?;
        Ok(Self {
            source,
            helper: fixed_helper,
            pinned_helper: Arc::new(file),
            helper_metadata: metadata,
            sha256: expected_sha256,
            limits,
            runtime_roots,
            toolchains,
            copy_mutations,
        })
    }
    pub(super) fn prepare(
        &self,
        grant: &ConfirmedSourceGrant,
    ) -> Result<PreparedWorkspace, FactoryError> {
        self.verify_helper()?;
        self.verify_source(grant)?;
        let mode = if grant.policy.write_allowed {
            SandboxMode::WorkspaceWrite
        } else {
            SandboxMode::ReadOnly
        };
        let prepared = match &self.toolchains {
            Some(plan) => PreparedWorkspace::prepare_with_toolchains(
                &self.source,
                &self.helper,
                mode,
                &self.runtime_roots,
                self.limits,
                plan,
            ),
            None => PreparedWorkspace::prepare(
                &self.source,
                &self.helper,
                mode,
                &self.runtime_roots,
                self.limits,
            ),
        }
        .map_err(|_| FactoryError::Workspace)?;
        // Core pins/copies helper and runtime under its existing auth registry
        // boundary. Hash the actual staged helper before returning any inputs.
        let copied =
            open_path(prepared.helper_path(), false).map_err(|_| FactoryError::Workspace)?;
        let metadata = copied.metadata().map_err(|_| FactoryError::Workspace)?;
        verify_bytes(prepared.helper_path(), &copied, &metadata, &self.sha256)?;
        self.verify_helper()?;
        self.verify_source(grant)?;
        Ok(prepared)
    }
    fn verify_source(&self, grant: &ConfirmedSourceGrant) -> Result<(), FactoryError> {
        let source = open_path(&self.source, true).map_err(|_| FactoryError::Binding)?;
        let metadata = source.metadata().map_err(|_| FactoryError::Binding)?;
        if self.source != grant.policy.source_path
            || metadata.dev() != grant.policy.source_identity.device.get()
            || metadata.ino() != grant.policy.source_identity.inode.get()
        {
            return Err(FactoryError::Binding);
        }
        Ok(())
    }
    fn verify_helper(&self) -> Result<(), FactoryError> {
        let current = open_path(&self.helper, false).map_err(|_| FactoryError::Workspace)?;
        let metadata = current.metadata().map_err(|_| FactoryError::Workspace)?;
        if !same(&metadata, &self.helper_metadata) {
            return Err(FactoryError::Workspace);
        }
        verify_bytes(
            &self.helper,
            &self.pinned_helper,
            &self.helper_metadata,
            &self.sha256,
        )
    }
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn verify_bytes(
    path: &Path,
    file: &File,
    expected: &Metadata,
    hash: &[u8; 32],
) -> Result<(), FactoryError> {
    polaris_core::isolated_run::with_protected_helper_read(path, file, |file| {
        verify_bytes_locked(file, expected, hash)
    })
    .map_err(|_| FactoryError::Workspace)?
}
fn verify_bytes_locked(
    file: &File,
    expected: &Metadata,
    hash: &[u8; 32],
) -> Result<(), FactoryError> {
    let before = file.metadata().map_err(|_| FactoryError::Workspace)?;
    if !same(&before, expected)
        || !before.is_file()
        || before.nlink() != 1
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o022 != 0
        || before.mode() & 0o111 == 0
        || before.len() > MAX_HELPER_BYTES
    {
        return Err(FactoryError::Workspace);
    }
    // read_at avoids shared file-offset mutation across cloned recipe FDs.
    let mut body = Vec::new();
    let mut buffer = [0; 16384];
    loop {
        let n = match file.read_at(&mut buffer, body.len() as u64) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            value => value.map_err(|_| FactoryError::Workspace)?,
        };
        if n == 0 {
            break;
        }
        if body.len() as u64 + n as u64 > MAX_HELPER_BYTES {
            return Err(FactoryError::Bounds);
        }
        body.extend_from_slice(&buffer[..n]);
    }
    let expected_hash = hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if polaris_core::conversation_state::content_hash(&body) != expected_hash
        || !same(
            &before,
            &file.metadata().map_err(|_| FactoryError::Workspace)?,
        )
    {
        return Err(FactoryError::Workspace);
    }
    Ok(())
}
/// Fixed manifest/source paths only. Every component is opened relative to its
/// predecessor FD; no canonicalize or final-component-only symlink fallback.
fn open_path(path: &Path, directory: bool) -> io::Result<File> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(io::Error::other("invalid preparation path"));
    }
    let names = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(v) => Some(v),
            _ => None,
        })
        .collect::<Vec<_>>();
    if names.is_empty() || names.len() > 128 {
        return Err(io::Error::other("invalid preparation path"));
    }
    let mut file = File::open("/")?;
    for (i, name) in names.iter().enumerate() {
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| io::Error::other("invalid preparation path"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory || i + 1 < names.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}
