//! Frozen postimages and create-only publication. The host retains the project
//! lease and has reaped every writer before invoking these operations.
use crate::{
    Error,
    definition::{Limits, relative},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageEntry {
    Directory {
        path: String,
    },
    File {
        path: String,
        bytes: u64,
        sha256: String,
        executable: bool,
    },
    Link {
        path: String,
        target: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub entries: Vec<ImageEntry>,
    pub bytes: u64,
}

impl Inventory {
    pub fn digest(&self) -> Result<String, Error> {
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| Error::RecoveryRequired)?)
        ))
    }
}

#[cfg(unix)]
mod walk;
#[cfg(unix)]
pub use walk::inspect_read_only;
#[cfg(unix)]
pub use walk::{inspect, inspect_admitted};

/// Publish exactly one pre-inventoried image without replacing a destination.
/// Each call needs a durable per-path intent; its caller fsyncs the receipt
/// before proceeding to the next path. Cross-device moves are refused.
#[cfg(target_os = "macos")]
pub fn create_only(
    target: &Path,
    stage: &Path,
    path: &str,
    expected: &Inventory,
    limits: &Limits,
) -> Result<(), Error> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
        os::unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    };
    if !relative(path) || inspect(&stage.join(path), limits)? != *expected {
        return Err(Error::StalePlan);
    }
    fn open(path: &Path) -> Result<File, Error> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| Error::RecoveryRequired)
    }
    fn parents(root: &Path, suffix: &Path, create: bool) -> Result<File, Error> {
        let mut directory = open(root)?;
        for component in suffix.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(Error::StalePlan);
            };
            let name = CString::new(name.as_bytes()).map_err(|_| Error::StalePlan)?;
            let mut fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0
                && create
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    return Err(Error::RecoveryRequired);
                }
                directory.sync_all().map_err(|_| Error::RecoveryRequired)?;
                fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            if fd < 0 {
                return Err(Error::RecoveryRequired);
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        Ok(directory)
    }
    if target.canonicalize().map_err(|_| Error::RecoveryRequired)? != target
        || stage.canonicalize().map_err(|_| Error::RecoveryRequired)? != stage
    {
        return Err(Error::StalePlan);
    }
    let path = Path::new(path);
    let parent = path.parent().ok_or(Error::StalePlan)?;
    let source = parents(stage, parent, false)?;
    let destination = parents(target, parent, true)?;
    if source
        .metadata()
        .map_err(|_| Error::RecoveryRequired)?
        .dev()
        != destination
            .metadata()
            .map_err(|_| Error::RecoveryRequired)?
            .dev()
    {
        return Err(Error::RecoveryRequired);
    }
    let name = CString::new(path.file_name().ok_or(Error::StalePlan)?.as_bytes())
        .map_err(|_| Error::StalePlan)?;
    if unsafe {
        libc::renameatx_np(
            source.as_raw_fd(),
            name.as_ptr(),
            destination.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(Error::RecoveryRequired);
    }
    source
        .sync_all()
        .and_then(|_| destination.sync_all())
        .map_err(|_| Error::RecoveryRequired)?;
    if inspect(&target.join(path), limits)? != *expected {
        return Err(Error::RecoveryRequired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_bytes_and_escaping_links_do_not_match_a_frozen_inventory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), "first").unwrap();
        let limits = Limits::default();
        let physical = root.path().canonicalize().unwrap();
        let first = inspect(&physical, &limits).unwrap();
        fs::write(root.path().join("file"), "second").unwrap();
        assert_ne!(first, inspect(&physical, &limits).unwrap());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../outside", root.path().join("escape")).unwrap();
            assert!(inspect(&physical, &limits).is_err());
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn atomic_publication_never_replaces_existing_user_bytes() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let stage = root.join("stage");
        let target = root.join("target");
        fs::create_dir(&stage).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(stage.join("file"), "new").unwrap();
        fs::write(target.join("file"), "user").unwrap();
        let limits = Limits::default();
        let image = inspect(&stage.join("file"), &limits).unwrap();
        assert!(create_only(&target, &stage, "file", &image, &limits).is_err());
        assert_eq!(fs::read(target.join("file")).unwrap(), b"user");
        assert_eq!(fs::read(stage.join("file")).unwrap(), b"new");
        fs::write(stage.join("other"), "new").unwrap();
        create_only(&target, &stage, "other", &image, &limits).unwrap();
        assert_eq!(fs::read(target.join("other")).unwrap(), b"new");
    }
}
