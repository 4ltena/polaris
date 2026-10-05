//! One project-wide lock for turns, owned children and workspace mutations.
//!
//! Rewind used this lock before other management operations existed. Keep its
//! existing on-disk path so an older process and a newer process cannot each
//! believe they own a different project writer lock.

use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
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
}

impl ProjectMutationOwnerV1 {
    /// The shared production lock path is the original rewind writer lease.
    pub fn for_project(project_root: &Path) -> Result<Self, ProjectMutationError> {
        let state = crate::project::state_dir(project_root)?;
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

    pub fn try_mutation(&self) -> Result<ProjectMutationLeaseV1, ProjectMutationError> {
        let guard = self.lock(libc::LOCK_EX)?;
        let root = self.directory.metadata()?;
        Ok(ProjectMutationLeaseV1 {
            _guard: guard,
            store_root: self.store_root.clone(),
            root_identity: (root.dev(), root.ino()),
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
        if meta.dev() != entry.st_dev as u64 || meta.ino() != entry.st_ino as u64 {
            return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
        }
        directory = child;
    }
    Ok(directory)
}

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

#[derive(Debug)]
struct LockGuard(File);

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
}

impl ProjectMutationLeaseV1 {
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

    pub(crate) fn lock_identity(&self) -> std::io::Result<(u64, u64)> {
        let metadata = self._guard.0.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
