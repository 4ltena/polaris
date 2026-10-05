//! Explicit, disposable-root acceptance probe for Linux filesystem primitives.
//!
//! This does not run in ordinary CI. An operator must provide an empty directory
//! on the same filesystem as the checked-out project after approving the mount
//! policy. The test only creates and removes one random child in that directory.

#![cfg(target_os = "linux")]

use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, Metadata};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

fn refuse(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn component(name: &OsStr) -> std::io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| refuse("invalid path component"))
}

fn open_directory_component(parent: &File, name: &OsStr) -> std::io::Result<File> {
    let name = component(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Resolves each component below the real root with `O_NOFOLLOW`; no mutable
/// path is consulted after the returned descriptor is pinned.
fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(refuse("refusing filesystem root"));
    }
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => directory = open_directory_component(&directory, name)?,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(refuse("path must contain only normal absolute components"));
            }
        }
    }
    Ok(directory)
}

fn open_directory_stream(directory: &File) -> std::io::Result<*mut libc::DIR> {
    let current = c".";
    let fresh = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            current.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fresh < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(fresh) };
    if stream.is_null() {
        unsafe { libc::close(fresh) };
        return Err(std::io::Error::last_os_error());
    }
    Ok(stream)
}

fn directory_is_empty(directory: &File) -> std::io::Result<bool> {
    let stream = open_directory_stream(directory)?;
    loop {
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::closedir(stream) };
            return if error.raw_os_error() == Some(0) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            unsafe { libc::closedir(stream) };
            return Ok(false);
        }
    }
}

fn directory_has_only(directory: &File, expected: &CStr) -> std::io::Result<bool> {
    let stream = open_directory_stream(directory)?;
    let mut found = false;
    loop {
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::closedir(stream) };
            return if error.raw_os_error() == Some(0) {
                Ok(found)
            } else {
                Err(error)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if found || name != expected {
            unsafe { libc::closedir(stream) };
            return Ok(false);
        }
        found = true;
    }
}

fn project_root() -> std::io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| refuse("cannot derive project root"))?
        .canonicalize()
}

struct DisposableRoot {
    directory: File,
    identity: (u64, u64),
    device: u64,
}

impl DisposableRoot {
    fn verify(&self) -> std::io::Result<()> {
        let metadata = self.directory.metadata()?;
        if !metadata.is_dir()
            || identity(&metadata) != self.identity
            || metadata.dev() != self.device
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(refuse("disposable root identity changed"));
        }
        Ok(())
    }

    fn require_empty(&self) -> std::io::Result<()> {
        if !directory_is_empty(&self.directory)? {
            return Err(refuse("root must be empty"));
        }
        Ok(())
    }
}

fn disposable_root(path: &Path) -> std::io::Result<DisposableRoot> {
    let directory = open_directory_nofollow(path)?;
    let metadata = directory.metadata()?;
    let project = fs::metadata(project_root()?)?;
    if identity(&metadata) == identity(&project) {
        return Err(refuse("refusing project root"));
    }
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(refuse("root must be owned by the effective user"));
    }
    if metadata.dev() != project.dev() {
        return Err(refuse("root is not on the project filesystem"));
    }
    let root = DisposableRoot {
        identity: identity(&metadata),
        device: metadata.dev(),
        directory,
    };
    root.verify()?;
    root.require_empty()?;
    Ok(root)
}

fn random_name() -> std::io::Result<CString> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(std::io::Error::other)?;
    CString::new(format!(
        "polaris-linux-fs-acceptance-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
    .map_err(|_| refuse("generated invalid child name"))
}

struct OwnedChild {
    directory: File,
    name: CString,
    identity: (u64, u64),
    device: u64,
}

impl OwnedChild {
    fn create(root: &DisposableRoot, name: CString) -> std::io::Result<Self> {
        root.verify()?;
        root.require_empty()?;
        if unsafe { libc::mkdirat(root.directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        root.directory.sync_all()?;
        let directory =
            open_directory_component(&root.directory, OsStr::from_bytes(name.as_bytes()))?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.dev() != root.device
        {
            return Err(refuse("created child has unexpected identity"));
        }
        Ok(Self {
            identity: identity(&metadata),
            device: metadata.dev(),
            directory,
            name,
        })
    }

    /// Deletes only the original, empty child. Any replacement, mutation or
    /// identity failure is retained for manual inspection.
    fn remove_if_unchanged(&self, root: &DisposableRoot) -> std::io::Result<()> {
        root.verify()?;
        let metadata = self.directory.metadata()?;
        if !metadata.is_dir()
            || identity(&metadata) != self.identity
            || metadata.dev() != self.device
            || metadata.uid() != unsafe { libc::geteuid() }
            || !directory_is_empty(&self.directory)?
            || !directory_has_only(&root.directory, self.name.as_c_str())?
        {
            return Err(refuse("owned child changed; retaining evidence"));
        }
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                root.directory.as_raw_fd(),
                self.name.as_ptr(),
                named.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(refuse("owned child name changed; retaining evidence"));
        }
        let named = unsafe { named.assume_init() };
        if named.st_dev != self.identity.0 || named.st_ino != self.identity.1 {
            return Err(refuse("owned child name replaced; retaining evidence"));
        }
        if unsafe {
            libc::unlinkat(
                root.directory.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        root.directory.sync_all()
    }
}

#[test]
#[ignore = "requires an explicitly approved empty disposable directory"]
fn linux_filesystem_acceptance() {
    let root = std::env::var_os("POLARIS_ACCEPTANCE_FS_ROOT")
        .map(PathBuf::from)
        .expect("POLARIS_ACCEPTANCE_FS_ROOT is required");
    let root = disposable_root(&root).expect("unsafe disposable root");
    let child = OwnedChild::create(&root, random_name().expect("random child name"))
        .expect("create owned child exclusively");
    child
        .remove_if_unchanged(&root)
        .expect("remove unchanged owned child");
    root.require_empty().expect("root is empty after cleanup");
}

#[test]
fn disposable_root_refuses_nonempty_and_symlink_ancestor() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("sentinel"), b"keep").unwrap();
    assert!(disposable_root(temporary.path()).is_err());
    fs::remove_file(temporary.path().join("sentinel")).unwrap();
    let real = temporary.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = temporary.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(disposable_root(&link).is_err());
}

#[test]
fn replaced_child_is_retained() {
    let temporary = tempfile::tempdir().unwrap();
    let metadata = fs::metadata(temporary.path()).unwrap();
    let root = DisposableRoot {
        directory: File::open(temporary.path()).unwrap(),
        identity: identity(&metadata),
        device: metadata.dev(),
    };
    let child = OwnedChild::create(&root, CString::new("owned").unwrap()).unwrap();
    fs::rename(
        temporary.path().join("owned"),
        temporary.path().join("moved"),
    )
    .unwrap();
    fs::create_dir(temporary.path().join("owned")).unwrap();
    assert!(child.remove_if_unchanged(&root).is_err());
    assert!(temporary.path().join("owned").is_dir());
    assert!(temporary.path().join("moved").is_dir());
}

#[test]
fn changed_root_after_prior_empty_read_retains_child() {
    let temporary = tempfile::tempdir().unwrap();
    let metadata = fs::metadata(temporary.path()).unwrap();
    let root = DisposableRoot {
        directory: File::open(temporary.path()).unwrap(),
        identity: identity(&metadata),
        device: metadata.dev(),
    };
    root.require_empty().unwrap();
    let child = OwnedChild::create(&root, CString::new("owned").unwrap()).unwrap();
    fs::create_dir(temporary.path().join("unexpected")).unwrap();
    assert!(child.remove_if_unchanged(&root).is_err());
    assert!(temporary.path().join("owned").is_dir());
    assert!(temporary.path().join("unexpected").is_dir());
}
