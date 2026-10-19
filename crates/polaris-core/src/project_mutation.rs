//! One project-wide lock for turns, owned children and workspace mutations.
//!
//! Rewind used this lock before other management operations existed. Keep its
//! existing on-disk path so an older process and a newer process cannot each
//! believe they own a different project writer lock.

#[cfg(unix)]
use std::{
    ffi::CString,
    fs::{self, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, thiserror::Error)]
pub enum ProjectMutationError {
    #[error("project writer is busy")]
    Busy,
    #[error("invalid project mutation lock store: {0}")]
    InvalidStore(PathBuf),
    #[error("cannot access project mutation lock: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct ProjectMutationOwnerV1 {
    store_root: PathBuf,
    directory: File,
    #[cfg(all(test, windows))]
    windows: Arc<WindowsLeaseStore>,
}

#[cfg(unix)]
impl ProjectMutationOwnerV1 {
    /// The shared production lock path is the original rewind writer lease.
    pub fn for_project(project_root: &Path) -> Result<Self, ProjectMutationError> {
        let state = crate::project::state_dir(project_root)?;
        // State initialization belongs to this mutating host constructor, never
        // to composite inspection. Its strict readers require private state.
        let directory = open_directory_nofollow(&state)?;
        let before = directory.metadata()?;
        if before.uid() != unsafe { libc::geteuid() } || before.mode() & 0o7000 != 0 {
            return Err(ProjectMutationError::InvalidStore(state));
        }
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        directory.sync_all()?;
        let fresh = open_directory_nofollow(&state)?.metadata()?;
        if (before.dev(), before.ino()) != (fresh.dev(), fresh.ino())
            || fresh.mode() & 0o7777 != 0o700
        {
            return Err(ProjectMutationError::InvalidStore(state));
        }
        Self::at_store_root(&state.join("rewind-workspace"))
    }

    /// Used by the rewind owner after it has bound this store to the project.
    /// An independently chosen store must never be used for a production route.
    pub(crate) fn at_store_root(store_root: &Path) -> Result<Self, ProjectMutationError> {
        if let Err(error) = fs::create_dir(store_root)
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(error.into());
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(store_root)
            .map_err(|error| {
                if matches!(error.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENOTDIR)
                {
                    ProjectMutationError::InvalidStore(store_root.to_path_buf())
                } else {
                    ProjectMutationError::Io(error)
                }
            })?;
        let meta = directory.metadata()?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(ProjectMutationError::InvalidStore(store_root.to_path_buf()));
        }
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        let meta = directory.metadata()?;
        if meta.mode() & 0o7077 != 0 || meta.mode() & 0o700 != 0o700 {
            return Err(ProjectMutationError::InvalidStore(store_root.to_path_buf()));
        }
        let path = store_root.join("writer.lease");
        let file = open_lease(&directory, true)?;
        validate_file(&file, &path)?;
        let canonical_root = fs::canonicalize(store_root)?;
        let current = fs::metadata(&canonical_root)?;
        if current.dev() != meta.dev() || current.ino() != meta.ino() {
            return Err(ProjectMutationError::InvalidStore(store_root.to_path_buf()));
        }
        Ok(Self {
            store_root: canonical_root,
            directory,
        })
    }

    /// Existing lock only; diagnostic inspection never creates or chmods.
    pub(crate) fn open_existing(store_root: &Path) -> Result<Self, ProjectMutationError> {
        let directory = open_directory_nofollow(store_root)?;
        let meta = directory.metadata()?;
        if !meta.is_dir()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7777 != 0o700
            || fs::canonicalize(store_root)? != store_root
        {
            return Err(ProjectMutationError::InvalidStore(store_root.to_owned()));
        }
        validate_file(
            &open_lease(&directory, false)?,
            &store_root.join("writer.lease"),
        )?;
        Ok(Self {
            store_root: store_root.to_owned(),
            directory,
        })
    }

    /// Revalidate the actual held owner/EX lease and named lock for migration.
    /// Result<()> is an immediate check, never a stop or activation permit.
    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn verify_migration(
        &self,
        lease: &ProjectMutationLeaseV1,
        mount: &polaris_mcp::NativeMountIdentityV2,
    ) -> Result<(), ProjectMutationError> {
        if self.store_root != lease.store_root {
            return Err(ProjectMutationError::InvalidStore(self.store_root.clone()));
        }
        let named = open_directory_nofollow(&self.store_root)?;
        for file in [&self.directory, &named] {
            let metadata = file.metadata()?;
            if (metadata.dev(), metadata.ino()) != lease.root_identity
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o700
                || polaris_mcp::NativeMountIdentityV2::inspect(file)
                    .map_err(|_| ProjectMutationError::InvalidStore(self.store_root.clone()))?
                    != *mount
            {
                return Err(ProjectMutationError::InvalidStore(self.store_root.clone()));
            }
        }
        let named_lock = open_lease(&named, false)?;
        let expected = lease._guard.0.metadata()?;
        for file in [&lease._guard.0, &named_lock] {
            validate_file(file, &self.store_root.join("writer.lease"))?;
            let metadata = file.metadata()?;
            if (metadata.dev(), metadata.ino()) != (expected.dev(), expected.ino())
                || polaris_mcp::NativeMountIdentityV2::inspect(file)
                    .map_err(|_| ProjectMutationError::InvalidStore(self.store_root.clone()))?
                    != *mount
            {
                return Err(ProjectMutationError::InvalidStore(self.store_root.clone()));
            }
        }
        Ok(())
    }

    pub fn try_mutation(&self) -> Result<ProjectMutationLeaseV1, ProjectMutationError> {
        let guard = self.lock(libc::LOCK_EX)?;
        let root = self.directory.metadata()?;
        Ok(ProjectMutationLeaseV1 {
            _guard: guard,
            store_root: self.store_root.clone(),
            root_identity: (root.dev(), root.ino()),
            instance: Arc::new(()),
        })
    }

    pub fn try_activity(&self) -> Result<ProjectActivityLeaseV1, ProjectMutationError> {
        let guard = self.lock(libc::LOCK_SH)?;
        let root = self.directory.metadata()?;
        Ok(ProjectActivityLeaseV1 {
            _guard: guard,
            store_root: self.store_root.clone(),
            root_identity: (root.dev(), root.ino()),
        })
    }

    fn lock(&self, mode: libc::c_int) -> Result<LockGuard, ProjectMutationError> {
        let path = self.store_root.join("writer.lease");
        let file = open_lease(&self.directory, false)?;
        validate_file(&file, &path)?;
        if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                Err(ProjectMutationError::Busy)
            } else {
                Err(ProjectMutationError::Io(error))
            };
        }
        Ok(LockGuard(file))
    }
}

#[cfg(unix)]
fn open_lease(directory: &File, create: bool) -> Result<File, ProjectMutationError> {
    let name = CString::new("writer.lease").expect("fixed lock name");
    let flags =
        libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW | if create { libc::O_CREAT } else { 0 };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn validate_file(file: &File, path: &Path) -> Result<(), ProjectMutationError> {
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7177 != 0
    {
        return Err(ProjectMutationError::InvalidStore(path.to_path_buf()));
    }
    Ok(())
}

/// Pin every component rather than letting a parent symlink redirect an open.
/// Open a read-only directory capability without following path components.
/// Callers still validate ownership, privacy and their expected identity.
#[cfg(unix)]
pub fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;
    if !path.is_absolute() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => CString::new(name.as_bytes())
                .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?,
            _ => return Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        };
        let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                entry.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let entry = unsafe { entry.assume_init() };
        if entry.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let child = unsafe { File::from_raw_fd(fd) };
        let meta = child.metadata()?;
        if meta.dev() != entry.st_dev as u64 || meta.ino() != entry.st_ino {
            return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
        }
        directory = child;
    }
    Ok(directory)
}

#[cfg(unix)]
pub(crate) fn directory_names_bounded(
    directory: &File,
    limit: usize,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;
    // A new open description has an independent directory offset.
    let dot = c".";
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    let mut names = Vec::new();
    loop {
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().unwrap_or(0) != 0 {
                return Err(error);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() >= limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "directory entry limit exceeded",
            ));
        }
        names.push(std::ffi::OsString::from_vec(name.to_vec()));
    }
    names.sort();
    Ok(names)
}

#[cfg(not(all(test, windows)))]
#[derive(Debug)]
struct LockGuard(File);

#[cfg(all(test, windows))]
#[derive(Debug)]
struct LockGuard(WindowsLeaseGuard);

#[cfg(unix)]
impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Held across one workspace mutation and released only after recovery state
/// and the corresponding effect record are durable.
#[derive(Debug)]
pub struct ProjectMutationLeaseV1 {
    _guard: LockGuard,
    store_root: PathBuf,
    root_identity: (u64, u64),
    instance: Arc<()>,
}

/// An owned identity for one live acquisition, not a lock or mutation grant.
/// Keeping the private marker allocated prevents address reuse after release.
/// An owner may move its lease and this non-Clone witness into the same token.
pub(crate) struct ProjectMutationLeaseBindingV1 {
    instance: Arc<()>,
}
impl ProjectMutationLeaseBindingV1 {
    pub(crate) fn require_same_lease(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ProjectMutationError> {
        if Arc::ptr_eq(&self.instance, &lease.instance) {
            Ok(())
        } else {
            Err(ProjectMutationError::InvalidStore(lease.store_root.clone()))
        }
    }
}

impl ProjectMutationLeaseV1 {
    pub(crate) fn bind_identity(&self) -> ProjectMutationLeaseBindingV1 {
        ProjectMutationLeaseBindingV1 {
            instance: Arc::clone(&self.instance),
        }
    }

    /// Lets a project-scoped effect journal reject a lease from another project.
    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn store_identity(&self) -> (u64, u64) {
        self.root_identity
    }
}

/// Held by a running root turn, its owned children or a writing sandbox.
/// Multiple activities may coexist, but they exclude a mutation lease.
#[derive(Debug)]
pub struct ProjectActivityLeaseV1 {
    _guard: LockGuard,
    store_root: PathBuf,
    root_identity: (u64, u64),
}

impl ProjectActivityLeaseV1 {
    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    pub fn store_identity(&self) -> (u64, u64) {
        self.root_identity
    }

    #[cfg(unix)]
    pub(crate) fn lock_identity(&self) -> std::io::Result<(u64, u64)> {
        let metadata = self._guard.0.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

/// Borrowed inspection authority; activity never becomes mutation authority.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ProjectReadLeaseV1<'a> {
    Mutation(&'a ProjectMutationLeaseV1),
    Activity(&'a ProjectActivityLeaseV1),
}
impl<'a> ProjectReadLeaseV1<'a> {
    pub(crate) fn store_root(self) -> &'a Path {
        match self {
            Self::Mutation(v) => v.store_root(),
            Self::Activity(v) => v.store_root(),
        }
    }
    pub(crate) fn store_identity(self) -> (u64, u64) {
        match self {
            Self::Mutation(v) => v.store_identity(),
            Self::Activity(v) => v.store_identity(),
        }
    }
    pub(crate) fn mutation(self) -> Result<&'a ProjectMutationLeaseV1, ProjectMutationError> {
        match self {
            Self::Mutation(v) => Ok(v),
            Self::Activity(_) => Err(ProjectMutationError::InvalidStore(
                self.store_root().to_owned(),
            )),
        }
    }
    #[cfg(unix)]
    pub(crate) fn verify(self, state: &Path) -> Result<(), ProjectMutationError> {
        let expected = state.join("rewind-workspace");
        let directory = open_directory_nofollow(&expected)?;
        let meta = directory.metadata()?;
        if self.store_root() != expected
            || self.store_identity() != (meta.dev(), meta.ino())
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7777 != 0o700
        {
            return Err(ProjectMutationError::InvalidStore(expected));
        }
        let held = match self {
            Self::Mutation(v) => &v._guard.0,
            Self::Activity(v) => &v._guard.0,
        };
        validate_file(held, &expected.join("writer.lease"))?;
        let named = open_lease(&directory, false)?;
        validate_file(&named, &expected.join("writer.lease"))?;
        let held_meta = held.metadata()?;
        let named_meta = named.metadata()?;
        if (held_meta.dev(), held_meta.ino()) != (named_meta.dev(), named_meta.ino()) {
            return Err(ProjectMutationError::InvalidStore(expected));
        }
        Ok(())
    }
}

#[cfg(not(unix))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "project mutation is unavailable on this platform",
    )
}

#[cfg(not(unix))]
impl ProjectMutationOwnerV1 {
    pub fn for_project(_project_root: &Path) -> Result<Self, ProjectMutationError> {
        Err(ProjectMutationError::Io(unsupported()))
    }

    #[cfg(not(all(test, windows)))]
    pub(crate) fn at_store_root(_store_root: &Path) -> Result<Self, ProjectMutationError> {
        Err(ProjectMutationError::Io(unsupported()))
    }

    #[cfg(not(all(test, windows)))]
    pub(crate) fn open_existing(_store_root: &Path) -> Result<Self, ProjectMutationError> {
        Err(ProjectMutationError::Io(unsupported()))
    }

    #[cfg(not(all(test, windows)))]
    pub fn try_mutation(&self) -> Result<ProjectMutationLeaseV1, ProjectMutationError> {
        Err(ProjectMutationError::Io(unsupported()))
    }

    #[cfg(not(all(test, windows)))]
    pub fn try_activity(&self) -> Result<ProjectActivityLeaseV1, ProjectMutationError> {
        Err(ProjectMutationError::Io(unsupported()))
    }
}

#[cfg(not(unix))]
pub fn open_directory_nofollow(_path: &Path) -> std::io::Result<File> {
    Err(unsupported())
}

#[cfg(not(unix))]
pub(crate) fn directory_names_bounded(
    _directory: &File,
    _limit: usize,
) -> std::io::Result<Vec<std::ffi::OsString>> {
    Err(unsupported())
}

#[cfg(not(unix))]
impl ProjectActivityLeaseV1 {
    pub(crate) fn lock_identity(&self) -> std::io::Result<(u64, u64)> {
        Err(unsupported())
    }
}

#[cfg(all(not(unix), not(all(test, windows))))]
impl ProjectReadLeaseV1<'_> {
    pub(crate) fn verify(self, _state: &Path) -> Result<(), ProjectMutationError> {
        Err(ProjectMutationError::Io(unsupported()))
    }
}

// Native owners stay unit-test-only until the complete Windows owner gates pass.
#[cfg(all(test, windows))]
use polaris_auth::protection::{WindowsFileIdentity, WindowsFileLock, WindowsPrivateRoot};

#[cfg(all(test, windows))]
struct WindowsLeaseStore {
    root: WindowsPrivateRoot,
    lease_identity: WindowsFileIdentity,
}

#[cfg(all(test, windows))]
impl std::fmt::Debug for WindowsLeaseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsLeaseStore")
            .field("root_identity", &self.root.identity())
            .field("lease_identity", &self.lease_identity)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, windows))]
#[derive(Debug)]
struct WindowsLeaseGuard {
    file: WindowsFileLock,
    store: Arc<WindowsLeaseStore>,
}

#[cfg(all(test, windows))]
fn windows_error(error: polaris_auth::AuthError, path: &Path) -> ProjectMutationError {
    match error {
        polaris_auth::AuthError::Busy => ProjectMutationError::Busy,
        polaris_auth::AuthError::Io(error) => ProjectMutationError::Io(error),
        _ => ProjectMutationError::InvalidStore(path.to_owned()),
    }
}

#[cfg(all(test, windows))]
fn windows_legacy_identity(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Compatibility metadata only. Authority checks use the complete FileIdInfo below.
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    if info.dwVolumeSerialNumber == 0 || index == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "ambiguous legacy file identity",
        ));
    }
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

#[cfg(all(test, windows))]
impl ProjectMutationOwnerV1 {
    pub(crate) fn at_store_root(store_root: &Path) -> Result<Self, ProjectMutationError> {
        use polaris_auth::AuthError;
        let root = match WindowsPrivateRoot::open_existing(store_root) {
            Ok(root) => root,
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                match WindowsPrivateRoot::create_new(store_root) {
                    Ok(root) => root,
                    Err(AuthError::Io(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        WindowsPrivateRoot::open_existing(store_root)
                            .map_err(|error| windows_error(error, store_root))?
                    }
                    Err(error) => return Err(windows_error(error, store_root)),
                }
            }
            Err(error) => return Err(windows_error(error, store_root)),
        };
        let name = std::ffi::OsStr::new("writer.lease");
        match root.open_file(name) {
            Ok(file) => drop(file),
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                match root.create_file(name) {
                    Ok(file) => {
                        file.flush()
                            .map_err(|error| windows_error(error, store_root))?;
                        // Drop the writable/DELETE handle before read-only reopening.
                        drop(file);
                    }
                    Err(AuthError::Io(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(windows_error(error, store_root)),
                }
            }
            Err(error) => return Err(windows_error(error, store_root)),
        }
        Self::from_windows_root(store_root, root)
    }

    pub(crate) fn open_existing(store_root: &Path) -> Result<Self, ProjectMutationError> {
        let root = WindowsPrivateRoot::open_existing(store_root)
            .map_err(|error| windows_error(error, store_root))?;
        Self::from_windows_root(store_root, root)
    }

    fn from_windows_root(
        store_root: &Path,
        root: WindowsPrivateRoot,
    ) -> Result<Self, ProjectMutationError> {
        use std::os::windows::io::AsHandle;
        let file = root
            .open_file(std::ffi::OsStr::new("writer.lease"))
            .map_err(|error| windows_error(error, store_root))?;
        let lease_identity = file.identity();
        root.revalidate()
            .map_err(|error| windows_error(error, store_root))?;
        let directory = File::from(root.directory().as_handle().try_clone_to_owned()?);
        windows_legacy_identity(&directory)?;
        Ok(Self {
            store_root: store_root.to_owned(),
            directory,
            windows: Arc::new(WindowsLeaseStore {
                root,
                lease_identity,
            }),
        })
    }

    pub fn try_mutation(&self) -> Result<ProjectMutationLeaseV1, ProjectMutationError> {
        let guard = self.windows_lock(true)?;
        Ok(ProjectMutationLeaseV1 {
            _guard: guard,
            store_root: self.store_root.clone(),
            root_identity: windows_legacy_identity(&self.directory)?,
            instance: Arc::new(()),
        })
    }

    pub fn try_activity(&self) -> Result<ProjectActivityLeaseV1, ProjectMutationError> {
        let guard = self.windows_lock(false)?;
        Ok(ProjectActivityLeaseV1 {
            _guard: guard,
            store_root: self.store_root.clone(),
            root_identity: windows_legacy_identity(&self.directory)?,
        })
    }

    fn windows_lock(&self, exclusive: bool) -> Result<LockGuard, ProjectMutationError> {
        let file = self
            .windows
            .root
            .open_file(std::ffi::OsStr::new("writer.lease"))
            .map_err(|error| windows_error(error, &self.store_root))?;
        if file.identity() != self.windows.lease_identity {
            return Err(ProjectMutationError::InvalidStore(self.store_root.clone()));
        }
        let file = if exclusive {
            file.try_lock_exclusive()
        } else {
            file.try_lock_shared()
        }
        .map_err(|error| windows_error(error, &self.store_root))?;
        let guard = WindowsLeaseGuard {
            file,
            store: Arc::clone(&self.windows),
        };
        guard.verify(&self.store_root)?;
        Ok(LockGuard(guard))
    }
}

#[cfg(all(test, windows))]
impl WindowsLeaseGuard {
    fn verify(&self, store_root: &Path) -> Result<(), ProjectMutationError> {
        self.file
            .revalidate()
            .map_err(|error| windows_error(error, store_root))?;
        self.store
            .root
            .revalidate()
            .map_err(|error| windows_error(error, store_root))?;
        let named_root = WindowsPrivateRoot::open_existing(store_root)
            .map_err(|error| windows_error(error, store_root))?;
        let named_file = named_root
            .open_file(std::ffi::OsStr::new("writer.lease"))
            .map_err(|error| windows_error(error, store_root))?;
        if named_root.identity() != self.store.root.identity()
            || self.file.identity() != self.store.lease_identity
            || named_file.identity() != self.file.identity()
        {
            return Err(ProjectMutationError::InvalidStore(store_root.to_owned()));
        }
        self.file
            .revalidate()
            .map_err(|error| windows_error(error, store_root))
    }
}

#[cfg(all(test, windows))]
impl ProjectReadLeaseV1<'_> {
    pub(crate) fn verify(self, state: &Path) -> Result<(), ProjectMutationError> {
        let expected = state.join("rewind-workspace");
        if self.store_root() != expected {
            return Err(ProjectMutationError::InvalidStore(expected));
        }
        let state_pin = WindowsPrivateRoot::open_existing(state)
            .map_err(|error| windows_error(error, state))?;
        let held = match self {
            Self::Mutation(lease) => &lease._guard.0,
            Self::Activity(lease) => &lease._guard.0,
        };
        held.verify(&expected)?;
        state_pin
            .revalidate()
            .map_err(|error| windows_error(error, state))
    }
}

#[cfg(all(test, windows))]
mod windows_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn lease_binding_moves_with_an_owned_confirmation_token() {
        struct Token {
            lease: ProjectMutationLeaseV1,
            reservation: ProjectMutationLeaseBindingV1,
        }
        let root = tempfile::tempdir().unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(root.path()).unwrap();
        let lease = owner.try_mutation().unwrap();
        let reservation = lease.bind_identity();
        let token = Token { lease, reservation };
        let moved = Box::new(token);
        moved.reservation.require_same_lease(&moved.lease).unwrap();
        assert!(matches!(
            owner.try_activity(),
            Err(ProjectMutationError::Busy)
        ));
        drop(moved);
        assert!(owner.try_activity().is_ok());
    }

    #[test]
    fn lease_binding_rejects_same_project_reacquisition_and_does_not_hold_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(root.path()).unwrap();
        let first = owner.try_mutation().unwrap();
        let witness = first.bind_identity();
        let identity = first.store_identity();
        drop(first);
        // The witness is not an extra lock holder. A fresh acquisition of the
        // same durable writer inode still cannot consume its stale reservation.
        let second = owner.try_mutation().unwrap();
        assert_eq!(second.store_identity(), identity);
        assert!(witness.require_same_lease(&second).is_err());
        drop(second);
        assert!(owner.try_activity().is_ok());
    }

    #[test]
    fn lease_binding_rejects_another_project_acquisition() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first_owner = ProjectMutationOwnerV1::at_store_root(first_root.path()).unwrap();
        let second_owner = ProjectMutationOwnerV1::at_store_root(second_root.path()).unwrap();
        let first = first_owner.try_mutation().unwrap();
        let second = second_owner.try_mutation().unwrap();
        assert!(first.bind_identity().require_same_lease(&second).is_err());
    }

    #[test]
    fn activities_share_and_exclude_mutation_until_every_holder_exits() {
        let root = tempfile::tempdir().unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(root.path()).unwrap();
        let first = owner.try_activity().unwrap();
        let second = owner.try_activity().unwrap();
        assert!(matches!(
            owner.try_mutation(),
            Err(ProjectMutationError::Busy)
        ));
        drop(first);
        assert!(matches!(
            owner.try_mutation(),
            Err(ProjectMutationError::Busy)
        ));
        drop(second);
        let writer = owner.try_mutation().unwrap();
        assert!(matches!(
            owner.try_activity(),
            Err(ProjectMutationError::Busy)
        ));
        drop(writer);
        assert!(owner.try_activity().is_ok());
    }

    #[test]
    fn symlinked_lock_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(root.path()).unwrap();
        fs::remove_file(root.path().join("writer.lease")).unwrap();
        std::os::unix::fs::symlink(root.path().join("other"), root.path().join("writer.lease"))
            .unwrap();
        assert!(owner.try_mutation().is_err());
    }

    #[test]
    fn symlinked_store_is_refused_without_changing_its_target_permissions() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            ProjectMutationOwnerV1::at_store_root(&link),
            Err(ProjectMutationError::InvalidStore(_))
        ));
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
    }
}
