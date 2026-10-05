//! Read-only validation of an empty disposable restart fixture parent.

use crate::project_mutation::{directory_names_bounded, open_directory_nofollow};
use std::{fs::File, os::unix::fs::MetadataExt, path::Path};

#[derive(Debug, thiserror::Error)]
pub enum L04ParentRefusal {
    #[error("parent_is_symlink")]
    ParentIsSymlink,
    #[error("parent_is_filesystem_root")]
    ParentIsFilesystemRoot,
    #[error("parent_is_project_root")]
    ParentIsProjectRoot,
    #[error("parent_nonempty_caller_owned")]
    ParentNonemptyCallerOwned,
    #[error("parent_on_different_device")]
    ParentOnDifferentDevice,
    #[error("invalid disposable parent: {0}")]
    InvalidParent(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct L04DisposableParent {
    directory: File,
    device: u64,
    inode: u64,
}

impl L04DisposableParent {
    pub fn directory(&self) -> &File {
        &self.directory
    }
    pub fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }
}

pub fn validate_disposable_parent_candidate(
    candidate: &Path,
    project_root: &Path,
    expected_device: u64,
) -> Result<L04DisposableParent, L04ParentRefusal> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let directory = open_directory_nofollow(candidate).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            L04ParentRefusal::ParentIsSymlink
        } else {
            L04ParentRefusal::InvalidParent(error)
        }
    })?;
    let meta = directory.metadata()?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"..".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let parent = unsafe { File::from_raw_fd(fd) }.metadata()?;
    if parent.dev() != meta.dev() || parent.ino() == meta.ino() {
        return Err(L04ParentRefusal::ParentIsFilesystemRoot);
    }
    let project = open_directory_nofollow(project_root)?.metadata()?;
    if (meta.dev(), meta.ino()) == (project.dev(), project.ino()) {
        return Err(L04ParentRefusal::ParentIsProjectRoot);
    }
    match directory_names_bounded(&directory, 1) {
        Ok(names) if names.is_empty() => {}
        Ok(_) => return Err(L04ParentRefusal::ParentNonemptyCallerOwned),
        Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
            return Err(L04ParentRefusal::ParentNonemptyCallerOwned);
        }
        Err(error) => return Err(error.into()),
    }
    if meta.dev() != expected_device {
        return Err(L04ParentRefusal::ParentOnDifferentDevice);
    }
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7077 != 0 {
        return Err(std::io::Error::other("disposable parent must be private and owned").into());
    }
    Ok(L04DisposableParent {
        directory,
        device: meta.dev(),
        inode: meta.ino(),
    })
}
