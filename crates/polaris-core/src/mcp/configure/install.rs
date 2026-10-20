//! Descriptor-relative independent Unity runtime copies and verified manifest publication.

use super::source::{ManifestEntry, Stamp, UnitySourceBinding};
use polaris_mcp::Error;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{CStr, OsStr, OsString},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path},
};

const MANIFEST: &str = ".polaris-unity-runtime-v1.json";

#[cfg(test)]
thread_local! {
    static COPY_FAULT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(super) fn set_copy_fault(hook: Option<Box<dyn FnOnce()>>) {
    COPY_FAULT.with(|current| *current.borrow_mut() = hook);
}

#[cfg(test)]
fn run_copy_fault() {
    if let Some(hook) = COPY_FAULT.with(|current| current.borrow_mut().take()) {
        hook();
    }
}

#[cfg(not(test))]
fn run_copy_fault() {}

#[derive(Serialize)]
struct RuntimeManifest<'a> {
    schema_version: u32,
    preset: &'static str,
    server_id: &'a str,
    operation_id: &'a str,
    source_manifest_sha256: &'a str,
    node_sha256: &'a str,
    entry_sha256: &'a str,
    selected_files: usize,
    selected_bytes: u64,
    fixed_exclusions: Vec<String>,
    hardlink_copy: &'static str,
}

#[derive(Clone)]
pub(super) struct InstalledRuntime {
    entries: Vec<InstalledEntry>,
    manifest_bytes: Vec<u8>,
    manifest_sha256: String,
}

#[derive(Clone)]
struct InstalledEntry {
    relative: Vec<u8>,
    directory: bool,
    mode: u32,
    bytes: u64,
    sha256: Option<String>,
    source_stamp: Stamp,
}

pub(super) fn populate(
    source: &UnitySourceBinding,
    destination_directory: &File,
    server_id: &str,
    operation_id: &str,
) -> Result<InstalledRuntime, String> {
    source.revalidate()?;
    run_copy_fault();
    let mut entries: Vec<_> = source.entries().iter().map(installed_entry).collect();
    entries.sort_by(|a, b| {
        let depth = |entry: &InstalledEntry| {
            Path::new(OsStr::from_bytes(&entry.relative))
                .components()
                .count()
        };
        depth(a)
            .cmp(&depth(b))
            .then_with(|| b.directory.cmp(&a.directory))
            .then_with(|| a.relative.cmp(&b.relative))
    });
    for entry in entries.iter().filter(|entry| entry.directory) {
        create_directory_at(destination_directory, &entry.relative)?;
    }
    for entry in entries.iter().filter(|entry| !entry.directory) {
        let relative = Path::new(OsStr::from_bytes(&entry.relative));
        let mut input = source.open_copy_source(relative)?;
        let before = input
            .metadata()
            .map_err(|_| "Unity source fileを確認できません")?;
        if !entry.source_stamp.matches(&before) {
            return Err("copy直前にUnity sourceが変更されました".into());
        }
        let mut output = create_file_at(destination_directory, &entry.relative)?;
        let digest = copy_hash(&mut input, &mut output, entry.bytes)?;
        if entry.sha256.as_deref() != Some(&digest) {
            return Err("copy中にUnity sourceが変更されました".into());
        }
        if !input
            .metadata()
            .is_ok_and(|metadata| entry.source_stamp.matches(&metadata))
        {
            return Err("copy中にUnity sourceが変更されました".into());
        }
        output
            .set_permissions(fs::Permissions::from_mode(entry.mode))
            .map_err(|_| "Unity runtime fileを保護できません")?;
        let copied = output
            .metadata()
            .map_err(|_| "Unity runtime fileを確認できません")?;
        if !copied.is_file()
            || copied.nlink() != 1
            || copied.uid() != unsafe { libc::geteuid() }
            || copied.mode() & 0o7777 != entry.mode
        {
            return Err("Unity runtime copyのidentityが無効です".into());
        }
    }
    source.revalidate()?;
    let fixed_exclusions = source
        .fixed_exclusion_paths()
        .into_iter()
        .map(|path| {
            path.to_str()
                .map(ToOwned::to_owned)
                .ok_or("Unity runtimeの固定除外pathを表現できません")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let manifest_bytes = serde_json::to_vec(&RuntimeManifest {
        schema_version: 1,
        preset: "unity-local-v2",
        server_id,
        operation_id,
        source_manifest_sha256: source.manifest_sha256(),
        node_sha256: source.node_sha256(),
        entry_sha256: source.entry_sha256(),
        selected_files: source.file_count(),
        selected_bytes: source.total_bytes(),
        fixed_exclusions,
        hardlink_copy: "exact-esbuild-pair-to-two-independent-files",
    })
    .map_err(|_| "Unity runtime manifestを作成できません")?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    let mut manifest = create_file_at(destination_directory, MANIFEST.as_bytes())?;
    manifest
        .write_all(&manifest_bytes)
        .map_err(|_| "Unity runtime manifestを書けません")?;
    let installed = InstalledRuntime {
        entries,
        manifest_bytes,
        manifest_sha256,
    };
    installed
        .verify(destination_directory)
        .map_err(|_| "Unity runtime copyを検証できません")?;
    Ok(installed)
}

impl InstalledRuntime {
    pub(super) fn verify(&self, root: &File) -> Result<(), Error> {
        let metadata = root.metadata().map_err(|_| Error::Storage)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(Error::Protection);
        }
        let mut expected: BTreeMap<Vec<u8>, &InstalledEntry> = self
            .entries
            .iter()
            .map(|entry| (entry.relative.clone(), entry))
            .collect();
        let manifest_key = MANIFEST.as_bytes().to_vec();
        let mut manifest_seen = false;
        verify_directory(
            root,
            Path::new(""),
            &mut expected,
            self,
            &manifest_key,
            &mut manifest_seen,
        )?;
        if !expected.is_empty() || !manifest_seen {
            return Err(Error::Protection);
        }
        Ok(())
    }

    pub(super) fn receipt_hash(&self) -> &str {
        &self.manifest_sha256
    }
}

fn installed_entry(entry: &ManifestEntry) -> InstalledEntry {
    InstalledEntry {
        relative: entry.relative.clone(),
        directory: entry.directory,
        mode: if entry.directory {
            0o700
        } else {
            let owner = entry.stamp.mode & 0o700;
            if owner & 0o400 == 0 {
                owner | 0o400
            } else {
                owner
            }
        },
        bytes: entry.stamp.bytes,
        sha256: entry.sha256.clone(),
        source_stamp: entry.stamp.clone(),
    }
}

fn copy_hash(input: &mut File, output: &mut File, expected: u64) -> Result<String, String> {
    input
        .seek(SeekFrom::Start(0))
        .map_err(|_| "Unity source fileを読めません")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| "Unity source fileを読めません")?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or("Unity source fileが大きすぎます")?;
        if total > expected {
            return Err("copy中にUnity sourceが変更されました".into());
        }
        digest.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|_| "Unity runtime fileを書けません")?;
    }
    if total != expected {
        return Err("copy中にUnity sourceが変更されました".into());
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn verify_directory(
    directory: &File,
    relative: &Path,
    expected: &mut BTreeMap<Vec<u8>, &InstalledEntry>,
    installed: &InstalledRuntime,
    manifest_key: &[u8],
    manifest_seen: &mut bool,
) -> Result<(), Error> {
    for name in names(directory)? {
        let child_relative = relative.join(&name);
        let key = child_relative.as_os_str().as_bytes().to_vec();
        if key == manifest_key {
            if *manifest_seen {
                return Err(Error::Protection);
            }
            *manifest_seen = true;
            let mut file = open_at(directory, &name, false)?;
            let metadata = file.metadata().map_err(|_| Error::Storage)?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
                || metadata.mode() & 0o7777 != 0o600
                || metadata.len() != installed.manifest_bytes.len() as u64
                || read_all(&mut file, metadata.len())? != installed.manifest_bytes
            {
                return Err(Error::Protection);
            }
            require_same_named_entry(directory, &name, false, &metadata)?;
            continue;
        }
        let entry = expected.remove(&key).ok_or(Error::Protection)?;
        let mut file = open_at(directory, &name, entry.directory)?;
        let metadata = file.metadata().map_err(|_| Error::Storage)?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != entry.mode
            || (!entry.directory && metadata.nlink() != 1)
            || (!entry.directory && metadata.len() != entry.bytes)
            || metadata.is_dir() != entry.directory
        {
            return Err(Error::Protection);
        }
        if entry.directory {
            verify_directory(
                &file,
                &child_relative,
                expected,
                installed,
                manifest_key,
                manifest_seen,
            )?;
        } else {
            let bytes = read_all(&mut file, entry.bytes)?;
            let digest = format!("{:x}", Sha256::digest(bytes));
            if entry.sha256.as_deref() != Some(&digest) {
                return Err(Error::Protection);
            }
        }
        require_same_named_entry(directory, &name, entry.directory, &metadata)?;
    }
    Ok(())
}

fn require_same_named_entry(
    parent: &File,
    name: &OsStr,
    directory: bool,
    expected: &std::fs::Metadata,
) -> Result<(), Error> {
    let reopened = open_at(parent, name, directory)?;
    let current = reopened.metadata().map_err(|_| Error::Storage)?;
    if (
        current.dev(),
        current.ino(),
        current.uid(),
        current.mode(),
        current.nlink(),
        current.len(),
    ) != (
        expected.dev(),
        expected.ino(),
        expected.uid(),
        expected.mode(),
        expected.nlink(),
        expected.len(),
    ) {
        return Err(Error::Protection);
    }
    Ok(())
}

fn create_directory_at(root: &File, relative: &[u8]) -> Result<(), String> {
    let (parent, name) = open_parent(root, relative)?;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| "Unity runtime relative pathが無効です")?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err("Unity runtime directoryを排他的に作成できません".into());
    }
    let directory = open_at(&parent, OsStr::from_bytes(name.as_bytes()), true)
        .map_err(|_| "Unity runtime directoryをdescriptorで再確認できません")?;
    let metadata = directory
        .metadata()
        .map_err(|_| "Unity runtime directoryを確認できません")?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err("Unity runtime directoryのidentityが無効です".into());
    }
    Ok(())
}

fn create_file_at(root: &File, relative: &[u8]) -> Result<File, String> {
    let (parent, name) = open_parent(root, relative)?;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| "Unity runtime relative pathが無効です")?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err("Unity runtime fileを排他的に作成できません".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|_| "Unity runtime fileを確認できません")?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err("Unity runtime fileのidentityが無効です".into());
    }
    Ok(file)
}

fn open_parent(root: &File, relative: &[u8]) -> Result<(File, OsString), String> {
    let relative = Path::new(OsStr::from_bytes(relative));
    let mut components = relative.components().peekable();
    let mut parent = root
        .try_clone()
        .map_err(|_| "Unity runtime root descriptorを複製できません")?;
    let mut leaf = None;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err("Unity runtime relative pathが無効です".into());
        };
        if components.peek().is_none() {
            leaf = Some(name.to_os_string());
            break;
        }
        parent = open_at(&parent, name, true)
            .map_err(|_| "Unity runtime parentをdescriptorで開けません")?;
        let metadata = parent
            .metadata()
            .map_err(|_| "Unity runtime parentを確認できません")?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err("Unity runtime parentのidentityが無効です".into());
        }
    }
    leaf.map(|leaf| (parent, leaf))
        .ok_or_else(|| "Unity runtime relative pathが空です".into())
}

fn read_all(file: &mut File, expected: u64) -> Result<Vec<u8>, Error> {
    file.seek(SeekFrom::Start(0)).map_err(|_| Error::Storage)?;
    let mut bytes = Vec::with_capacity(expected.min(1024 * 1024) as usize);
    file.take(expected + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Storage)?;
    if bytes.len() as u64 != expected {
        return Err(Error::Protection);
    }
    Ok(bytes)
}

fn open_at(parent: &File, name: &OsStr, directory: bool) -> Result<File, Error> {
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| Error::Protection)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        return Err(Error::Protection);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn names(directory: &File) -> Result<Vec<OsString>, Error> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Error::Protection);
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err(Error::Protection);
    }
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Dir(stream);
    let mut names = Vec::new();
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::dirent>::uninit();
        let mut found = std::ptr::null_mut();
        if unsafe { libc::readdir_r(stream.0, entry.as_mut_ptr(), &mut found) } != 0 {
            return Err(Error::Storage);
        }
        if found.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*found).d_name.as_ptr()) }.to_bytes();
        if !matches!(name, b"." | b"..") {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    if names.len() > 100_001 {
        return Err(Error::Limit);
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}
