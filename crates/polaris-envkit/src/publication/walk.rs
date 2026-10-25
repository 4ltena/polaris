//! Descriptor-anchored inventory. Child paths never follow an intermediate
//! symlink, even if an external process renames directories during inspection.
use super::{ImageEntry, Inventory};
use crate::{
    Error,
    definition::{Limits, relative},
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString, OsString},
    fs::{File, Metadata},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
        },
    },
    path::{Component, Path},
};

pub fn inspect(root: &Path, limits: &Limits) -> Result<Inventory, Error> {
    inspect_inner(root, limits, false, &|_, _| true)
}
/// Native SDKs are read-only inputs, not writable installation images. Their
/// files may have multiple links; every admitted byte is still hashed.
pub fn inspect_read_only(root: &Path, limits: &Limits) -> Result<Inventory, Error> {
    inspect_inner(root, limits, true, &|_, _| true)
}

pub fn inspect_admitted(
    root: &Path,
    limits: &Limits,
    admit: &dyn Fn(&Path, Option<&Metadata>) -> bool,
) -> Result<Inventory, Error> {
    inspect_inner(root, limits, false, admit)
}

fn inspect_inner(
    root: &Path,
    limits: &Limits,
    allow_links: bool,
    admit: &dyn Fn(&Path, Option<&Metadata>) -> bool,
) -> Result<Inventory, Error> {
    if !root.is_absolute()
        || root.parent().is_none()
        || root.canonicalize().map_err(|_| Error::RecoveryRequired)? != root
    {
        return Err(Error::RecoveryRequired);
    }
    let mut parent = File::open("/").map_err(|_| Error::RecoveryRequired)?;
    for component in root.parent().ok_or(Error::RecoveryRequired)?.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                parent = open(
                    &parent,
                    &CString::new(name.as_bytes()).map_err(|_| Error::RecoveryRequired)?,
                    true,
                )?
            }
            _ => return Err(Error::RecoveryRequired),
        }
    }
    let name = CString::new(root.file_name().ok_or(Error::RecoveryRequired)?.as_bytes())
        .map_err(|_| Error::RecoveryRequired)?;
    let mut result = Inventory {
        entries: Vec::new(),
        bytes: 0,
    };
    walk(
        &parent,
        &name,
        Path::new(""),
        limits,
        &mut result,
        allow_links,
        root,
        admit,
        0,
    )?;
    Ok(result)
}

fn open(parent: &File, name: &CStr, directory: bool) -> Result<File, Error> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        return Err(Error::RecoveryRequired);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn stat(parent: &File, name: &CStr) -> Result<libc::stat, Error> {
    let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(Error::RecoveryRequired);
    }
    Ok(unsafe { value.assume_init() })
}
fn same_stat(a: &libc::stat, b: &libc::stat) -> bool {
    (a.st_dev, a.st_ino, a.st_mode, a.st_size, a.st_nlink)
        == (b.st_dev, b.st_ino, b.st_mode, b.st_size, b.st_nlink)
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.mode(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.mode(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}

fn names(directory: &File, maximum: usize) -> Result<Vec<OsString>, Error> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(Error::RecoveryRequired);
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(Error::RecoveryRequired);
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
    let mut result = Vec::new();
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::dirent>::uninit();
        let mut found = std::ptr::null_mut();
        if unsafe { libc::readdir_r(stream.0, entry.as_mut_ptr(), &mut found) } != 0 {
            return Err(Error::RecoveryRequired);
        }
        if found.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*found).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if result.len() >= maximum {
            return Err(Error::RecoveryRequired);
        }
        result.push(OsString::from_vec(name.to_vec()));
    }
    result.sort();
    Ok(result)
}

// The descriptor, inventory budget and admission callback stay explicit across recursion.
#[allow(clippy::too_many_arguments)]
fn walk(
    parent: &File,
    name: &CStr,
    suffix: &Path,
    limits: &Limits,
    result: &mut Inventory,
    allow_links: bool,
    root: &Path,
    admit: &dyn Fn(&Path, Option<&Metadata>) -> bool,
    depth: usize,
) -> Result<(), Error> {
    if depth > 128 || result.entries.len() >= limits.archive_entries as usize {
        return Err(Error::RecoveryRequired);
    }
    if !admit(&root.join(suffix), None) {
        return Err(Error::RecoveryRequired);
    }
    let before = stat(parent, name)?;
    let path = suffix.to_str().ok_or(Error::RecoveryRequired)?.to_owned();
    match before.st_mode & libc::S_IFMT {
        libc::S_IFLNK => {
            if suffix.as_os_str().is_empty() {
                return Err(Error::RecoveryRequired);
            }
            let mut buffer = [0u8; 4097];
            let count = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count <= 0 || count as usize >= buffer.len() {
                return Err(Error::RecoveryRequired);
            }
            let target = std::str::from_utf8(&buffer[..count as usize])
                .map_err(|_| Error::RecoveryRequired)?;
            if target.chars().any(char::is_control) {
                return Err(Error::RecoveryRequired);
            }
            let mut depth = suffix
                .parent()
                .unwrap_or(Path::new(""))
                .components()
                .count();
            for component in Path::new(target).components() {
                match component {
                    Component::Normal(_) => depth += 1,
                    Component::CurDir => {}
                    Component::ParentDir if depth > 0 => depth -= 1,
                    _ => return Err(Error::RecoveryRequired),
                }
            }
            result.entries.push(ImageEntry::Link {
                path,
                target: target.into(),
            });
        }
        libc::S_IFREG => {
            if !allow_links && before.st_nlink != 1 {
                return Err(Error::RecoveryRequired);
            }
            let mut file = open(parent, name, false)?;
            let opened = file.metadata().map_err(|_| Error::RecoveryRequired)?;
            if !admit(&root.join(suffix), Some(&opened)) {
                return Err(Error::RecoveryRequired);
            }
            if opened.dev() != before.st_dev as u64
                || opened.ino() != before.st_ino
                || !opened.is_file()
            {
                return Err(Error::RecoveryRequired);
            }
            result.bytes = result
                .bytes
                .checked_add(opened.len())
                .ok_or(Error::RecoveryRequired)?;
            if result.bytes > limits.unpacked_bytes || opened.len() > limits.disk_bytes {
                return Err(Error::RecoveryRequired);
            }
            let mut digest = Sha256::new();
            let mut bytes = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|_| Error::RecoveryRequired)?;
                if count == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(count as u64)
                    .ok_or(Error::RecoveryRequired)?;
                if bytes > opened.len() {
                    return Err(Error::RecoveryRequired);
                }
                digest.update(&buffer[..count]);
            }
            if bytes != opened.len()
                || !same(
                    &opened,
                    &file.metadata().map_err(|_| Error::RecoveryRequired)?,
                )
            {
                return Err(Error::RecoveryRequired);
            }
            result.entries.push(ImageEntry::File {
                path,
                bytes,
                sha256: format!("{:x}", digest.finalize()),
                executable: opened.mode() & 0o111 != 0,
            });
        }
        libc::S_IFDIR => {
            let directory = open(parent, name, true)?;
            let opened = directory.metadata().map_err(|_| Error::RecoveryRequired)?;
            if !admit(&root.join(suffix), Some(&opened)) {
                return Err(Error::RecoveryRequired);
            }
            if opened.dev() != before.st_dev as u64 || opened.ino() != before.st_ino {
                return Err(Error::RecoveryRequired);
            }
            result.entries.push(ImageEntry::Directory { path });
            for child in names(
                &directory,
                (limits.archive_entries as usize).saturating_sub(result.entries.len()),
            )? {
                let suffix = suffix.join(&child);
                if !suffix.to_str().is_some_and(relative) {
                    return Err(Error::RecoveryRequired);
                }
                walk(
                    &directory,
                    &CString::new(child.as_bytes()).map_err(|_| Error::RecoveryRequired)?,
                    &suffix,
                    limits,
                    result,
                    allow_links,
                    root,
                    admit,
                    depth + 1,
                )?;
            }
            if !same(
                &opened,
                &directory.metadata().map_err(|_| Error::RecoveryRequired)?,
            ) {
                return Err(Error::RecoveryRequired);
            }
        }
        _ => return Err(Error::RecoveryRequired),
    }
    if !same_stat(&before, &stat(parent, name)?) {
        return Err(Error::RecoveryRequired);
    }
    Ok(())
}
