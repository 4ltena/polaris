//! Package-trusted, read-only Python runtime image.
//!
//! The production lock is embedded in this binary.  A receipt next to a
//! downloaded image is repair/UI metadata only and never participates in the
//! trust decision.  The shipped lock deliberately has no image until a signed
//! release DMG and its complete tree manifest are produced.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::FileExt, fs::MetadataExt, fs::OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const EMBEDDED_LOCK: &[u8] = include_bytes!(
    "../../../apps/macos/Sources/PolarisDesktop/Resources/readonly-runtime-images-v2.json"
);
const IMAGE_ID: &str = "python-e5-cat";
const MAX_LOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 100_000;
const MAX_TREE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_PLIST_BYTES: usize = 1024 * 1024;
const CHILD_IMAGE_FD: libc::c_int = 198;
static CHILD_IMAGE_FD_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum ManagedRuntimeError {
    #[error("managed runtime image is unavailable; repair is required")]
    Unavailable,
    #[error("managed runtime image or package lock is invalid")]
    Integrity,
    #[error("managed runtime image could not be mounted read-only")]
    Mount,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeImageLockV2 {
    schema_version: u32,
    images: Vec<RuntimeImageV2>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeImageV2 {
    id: String,
    version: String,
    platform: String,
    architecture: String,
    image_filename: String,
    image_size: u64,
    image_sha256: String,
    tree_manifest_sha256: String,
    tree_manifest: RuntimeTreeManifestV2,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    bundled_resource: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTreeManifestV2 {
    schema_version: u32,
    files: Vec<RuntimeTreeFileV2>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTreeFileV2 {
    path: String,
    size: u64,
    sha256: String,
    executable: bool,
}

#[derive(Debug)]
struct AttachedMount {
    mount_point: PathBuf,
    device: String,
    device_number: u64,
    readonly: bool,
}

trait MountBackend: Send + Sync {
    fn attach(&self, image: &File) -> Result<AttachedMount, ManagedRuntimeError>;
    fn validate_mount(&self, mount_point: &Path, device_number: u64) -> bool;
    fn detach(&self, device: &str);
}

#[derive(Debug)]
struct HdiutilBackend;

struct ReservedChildFd(libc::c_int);

impl Drop for ReservedChildFd {
    fn drop(&mut self) {
        // SAFETY: this descriptor is created and exclusively owned below.
        unsafe { libc::close(self.0) };
    }
}

fn attach_arguments() -> Vec<String> {
    vec![
        "attach".into(),
        "-plist".into(),
        "-readonly".into(),
        "-nobrowse".into(),
        "-noautoopen".into(),
        format!("/dev/fd/{CHILD_IMAGE_FD}"),
    ]
}

fn run_with_inherited_image_fd(
    program: &Path,
    args: &[impl AsRef<OsStr>],
    image: &File,
) -> Result<Output, ManagedRuntimeError> {
    let _serial = CHILD_IMAGE_FD_LOCK
        .lock()
        .map_err(|_| ManagedRuntimeError::Mount)?;
    // F_DUPFD_CLOEXEC both reserves the fixed descriptor in the parent and
    // keeps it from leaking unless the pre-exec hook explicitly clears it.
    // A higher result means the fixed slot was already occupied; fail closed
    // instead of overwriting an unrelated descriptor.
    // SAFETY: fcntl only duplicates the valid, open image descriptor.
    let duplicated =
        unsafe { libc::fcntl(image.as_raw_fd(), libc::F_DUPFD_CLOEXEC, CHILD_IMAGE_FD) };
    if duplicated < 0 {
        return Err(ManagedRuntimeError::Mount);
    }
    let reserved = ReservedChildFd(duplicated);
    if duplicated != CHILD_IMAGE_FD {
        return Err(ManagedRuntimeError::Mount);
    }

    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: after fork and before exec this performs only async-signal-safe
    // fcntl, making the one reserved descriptor inheritable by this child.
    unsafe {
        command.pre_exec(|| {
            if libc::fcntl(CHILD_IMAGE_FD, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|_| ManagedRuntimeError::Mount)?;
    // The child has inherited the descriptor. Close the parent's duplicate
    // before waiting; the original verified image File remains held by lease.
    drop(reserved);
    child
        .wait_with_output()
        .map_err(|_| ManagedRuntimeError::Mount)
}

impl MountBackend for HdiutilBackend {
    fn attach(&self, image: &File) -> Result<AttachedMount, ManagedRuntimeError> {
        // The already-open descriptor, rather than the mutable installation
        // pathname, is handed to the fixed system executable.
        let arguments = attach_arguments();
        let output = run_with_inherited_image_fd(Path::new("/usr/bin/hdiutil"), &arguments, image)?;
        if !output.status.success() || output.stdout.len() > MAX_PLIST_BYTES {
            return Err(ManagedRuntimeError::Mount);
        }
        let (device, mount_point) = parse_attach_plist(&output.stdout)?;
        inspect_mount(&device, &mount_point)
    }

    fn detach(&self, device: &str) {
        // Never widen cleanup to a mount point and never force detach.
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", device])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .status();
    }

    fn validate_mount(&self, mount_point: &Path, device_number: u64) -> bool {
        std::fs::metadata(mount_point)
            .ok()
            .is_some_and(|metadata| metadata.dev() == device_number)
            && mount_is_readonly(mount_point).unwrap_or(false)
    }
}

struct RuntimeInner {
    image_path: PathBuf,
    image_identity: (u64, u64),
    image: File,
    mount_root: File,
    python: File,
    mount_point: PathBuf,
    device: String,
    device_number: u64,
    manifest: RuntimeTreeManifestV2,
    backend: Arc<dyn MountBackend>,
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.backend.detach(&self.device);
    }
}

/// A mounted runtime identity.  Clones retain the image, mount root and Python
/// descriptors, so a child cannot outlive the identity that authorized it.
#[derive(Clone)]
pub struct ManagedRuntimeLease(Arc<RuntimeInner>);

impl ManagedRuntimeLease {
    pub fn acquire(home: &Path) -> Result<Self, ManagedRuntimeError> {
        Self::acquire_with(EMBEDDED_LOCK, home, Arc::new(HdiutilBackend))
    }

    fn acquire_with(
        lock_bytes: &[u8],
        home: &Path,
        backend: Arc<dyn MountBackend>,
    ) -> Result<Self, ManagedRuntimeError> {
        let image = select_image(lock_bytes)?;
        let image_path = home
            .join(".polaris/runtime-images")
            .join(&image.id)
            .join(&image.version)
            .join(&image.image_filename);
        let image_file = open_regular_nofollow(&image_path, false)?;
        let metadata = image_file
            .metadata()
            .map_err(|_| ManagedRuntimeError::Integrity)?;
        if metadata.len() != image.image_size || sha256_reader(&image_file)? != image.image_sha256 {
            return Err(ManagedRuntimeError::Integrity);
        }
        let image_identity = (metadata.dev(), metadata.ino());
        check_path_identity(&image_path, image_identity)?;
        let attached = backend.attach(&image_file)?;
        if !attached.readonly {
            backend.detach(&attached.device);
            return Err(ManagedRuntimeError::Mount);
        }
        let mount_root = match open_regular_nofollow(&attached.mount_point, true) {
            Ok(file) => file,
            Err(error) => {
                backend.detach(&attached.device);
                return Err(error);
            }
        };
        let root_metadata = match mount_root.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                backend.detach(&attached.device);
                return Err(ManagedRuntimeError::Mount);
            }
        };
        if root_metadata.dev() != attached.device_number {
            backend.detach(&attached.device);
            return Err(ManagedRuntimeError::Mount);
        }
        if verify_tree(&attached.mount_point, &image.tree_manifest).is_err() {
            backend.detach(&attached.device);
            return Err(ManagedRuntimeError::Integrity);
        }
        let python_path = attached.mount_point.join("runtime/bin/python3.11");
        let python = match open_regular_nofollow(&python_path, false) {
            Ok(file) => file,
            Err(error) => {
                backend.detach(&attached.device);
                return Err(error);
            }
        };
        let python_metadata = match python.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                backend.detach(&attached.device);
                return Err(ManagedRuntimeError::Integrity);
            }
        };
        if python_metadata.mode() & 0o111 == 0 {
            backend.detach(&attached.device);
            return Err(ManagedRuntimeError::Integrity);
        }
        let lease = Self(Arc::new(RuntimeInner {
            image_path,
            image_identity,
            image: image_file,
            mount_root,
            python,
            mount_point: attached.mount_point,
            device: attached.device,
            device_number: attached.device_number,
            manifest: image.tree_manifest,
            backend,
        }));
        lease.validate_before_spawn()?;
        Ok(lease)
    }

    pub fn python_path(&self) -> PathBuf {
        self.0.mount_point.join("runtime/bin/python3.11")
    }

    pub fn validate_before_spawn(&self) -> Result<(), ManagedRuntimeError> {
        check_path_identity(&self.0.image_path, self.0.image_identity)?;
        let image = self
            .0
            .image
            .metadata()
            .map_err(|_| ManagedRuntimeError::Integrity)?;
        if (image.dev(), image.ino()) != self.0.image_identity {
            return Err(ManagedRuntimeError::Integrity);
        }
        let root = self
            .0
            .mount_root
            .metadata()
            .map_err(|_| ManagedRuntimeError::Mount)?;
        if root.dev() != self.0.device_number
            || !self
                .0
                .backend
                .validate_mount(&self.0.mount_point, self.0.device_number)
        {
            return Err(ManagedRuntimeError::Mount);
        }
        let python = self
            .0
            .python
            .metadata()
            .map_err(|_| ManagedRuntimeError::Integrity)?;
        if python.dev() != self.0.device_number || python.mode() & 0o111 == 0 {
            return Err(ManagedRuntimeError::Integrity);
        }
        verify_tree(&self.0.mount_point, &self.0.manifest)
    }
}

fn select_image(bytes: &[u8]) -> Result<RuntimeImageV2, ManagedRuntimeError> {
    if bytes.len() > MAX_LOCK_BYTES {
        return Err(ManagedRuntimeError::Integrity);
    }
    let lock: RuntimeImageLockV2 =
        serde_json::from_slice(bytes).map_err(|_| ManagedRuntimeError::Integrity)?;
    if lock.schema_version != 2 {
        return Err(ManagedRuntimeError::Integrity);
    }
    let architecture = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return Err(ManagedRuntimeError::Unavailable);
    };
    let mut matches = lock.images.into_iter().filter(|entry| {
        entry.id == IMAGE_ID && entry.platform == "macos" && entry.architecture == architecture
    });
    let entry = matches.next().ok_or(ManagedRuntimeError::Unavailable)?;
    if matches.next().is_some()
        || !valid_component(&entry.version)
        || !valid_component(&entry.image_filename)
        || entry.image_size == 0
        || !valid_hash(&entry.image_sha256)
        || !valid_hash(&entry.tree_manifest_sha256)
        || entry.tree_manifest.schema_version != 1
        || entry.tree_manifest.files.is_empty()
        || entry.tree_manifest.files.len() > MAX_FILES
        || sha256_bytes(
            &serde_json::to_vec(&entry.tree_manifest)
                .map_err(|_| ManagedRuntimeError::Integrity)?,
        ) != entry.tree_manifest_sha256
        || entry.source_url.is_some() == entry.bundled_resource.is_some()
    {
        return Err(ManagedRuntimeError::Integrity);
    }
    validate_manifest(&entry.tree_manifest)?;
    Ok(entry)
}

fn validate_manifest(manifest: &RuntimeTreeManifestV2) -> Result<(), ManagedRuntimeError> {
    let mut paths = BTreeSet::new();
    let mut total = 0_u64;
    for file in &manifest.files {
        let path = Path::new(&file.path);
        if file.path.len() > 4096
            || path.as_os_str().is_empty()
            || path.components().count() > 64
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || !valid_hash(&file.sha256)
            || !paths.insert(file.path.clone())
        {
            return Err(ManagedRuntimeError::Integrity);
        }
        total = total
            .checked_add(file.size)
            .filter(|value| *value <= MAX_TREE_BYTES)
            .ok_or(ManagedRuntimeError::Integrity)?;
    }
    if !paths.contains("runtime/bin/python3.11")
        || !manifest
            .files
            .iter()
            .any(|file| file.path == "runtime/bin/python3.11" && file.executable)
    {
        return Err(ManagedRuntimeError::Integrity);
    }
    Ok(())
}

fn verify_tree(root: &Path, manifest: &RuntimeTreeManifestV2) -> Result<(), ManagedRuntimeError> {
    let expected = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    let expected_directories = manifest
        .files
        .iter()
        .flat_map(|file| {
            let mut directories = Vec::new();
            let mut parent = Path::new(&file.path).parent();
            while let Some(path) = parent {
                if !path.as_os_str().is_empty() {
                    directories.push(path.to_string_lossy().into_owned());
                }
                parent = path.parent();
            }
            directories
        })
        .collect::<BTreeSet<_>>();
    let mut observed_directories = BTreeSet::new();
    let mut pending = vec![(root.to_owned(), 0_usize)];
    let mut entries = 0_usize;
    while let Some((directory, depth)) = pending.pop() {
        if depth > 64 {
            return Err(ManagedRuntimeError::Integrity);
        }
        let metadata =
            std::fs::symlink_metadata(&directory).map_err(|_| ManagedRuntimeError::Integrity)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(ManagedRuntimeError::Integrity);
        }
        for entry in std::fs::read_dir(&directory).map_err(|_| ManagedRuntimeError::Integrity)? {
            let path = entry.map_err(|_| ManagedRuntimeError::Integrity)?.path();
            entries += 1;
            if entries > MAX_FILES {
                return Err(ManagedRuntimeError::Integrity);
            }
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|_| ManagedRuntimeError::Integrity)?;
            if metadata.file_type().is_symlink() {
                return Err(ManagedRuntimeError::Integrity);
            }
            if metadata.file_type().is_dir() {
                observed_directories.insert(
                    path.strip_prefix(root)
                        .map_err(|_| ManagedRuntimeError::Integrity)?
                        .to_string_lossy()
                        .into_owned(),
                );
                pending.push((path, depth + 1));
            } else if metadata.file_type().is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| ManagedRuntimeError::Integrity)?;
                observed.insert(relative.to_string_lossy().into_owned());
            } else {
                return Err(ManagedRuntimeError::Integrity);
            }
        }
    }
    if observed != expected || observed_directories != expected_directories {
        return Err(ManagedRuntimeError::Integrity);
    }
    for expected in &manifest.files {
        let path = root.join(&expected.path);
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|_| ManagedRuntimeError::Integrity)?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != expected.size
            || (metadata.mode() & 0o111 != 0) != expected.executable
            || sha256_path(&path)? != expected.sha256
        {
            return Err(ManagedRuntimeError::Integrity);
        }
    }
    Ok(())
}

fn open_regular_nofollow(path: &Path, directory: bool) -> Result<File, ManagedRuntimeError> {
    let mut options = OpenOptions::new();
    options.read(true);
    let mut flags = libc::O_NOFOLLOW;
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    options.custom_flags(flags);
    let file = options
        .open(path)
        .map_err(|_| ManagedRuntimeError::Integrity)?;
    let metadata = file
        .metadata()
        .map_err(|_| ManagedRuntimeError::Integrity)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(ManagedRuntimeError::Integrity);
    }
    Ok(file)
}

fn check_path_identity(path: &Path, expected: (u64, u64)) -> Result<(), ManagedRuntimeError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ManagedRuntimeError::Integrity)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || (metadata.dev(), metadata.ino()) != expected
    {
        return Err(ManagedRuntimeError::Integrity);
    }
    Ok(())
}

fn sha256_reader(file: &File) -> Result<String, ManagedRuntimeError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    let mut offset = 0_u64;
    loop {
        let read = file
            .read_at(&mut buffer, offset)
            .map_err(|_| ManagedRuntimeError::Integrity)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or(ManagedRuntimeError::Integrity)?;
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_path(path: &Path) -> Result<String, ManagedRuntimeError> {
    sha256_reader(&open_regular_nofollow(path, false)?)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !matches!(value, "." | "..")
        && !value.bytes().any(|byte| byte == b'/' || byte == 0)
}

fn parse_attach_plist(bytes: &[u8]) -> Result<(String, PathBuf), ManagedRuntimeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ManagedRuntimeError::Mount)?;
    let mount_marker = "<key>mount-point</key>";
    if !text.starts_with("<?xml") || text.matches(mount_marker).count() != 1 {
        return Err(ManagedRuntimeError::Mount);
    }
    // hdiutil normally emits one entity for the disk and another for the
    // mounted volume. Select both values from the one flat entity containing
    // mount-point instead of assuming the whole plist has one dev-entry.
    let mount_offset = text.find(mount_marker).ok_or(ManagedRuntimeError::Mount)?;
    let entity_start = text[..mount_offset]
        .rfind("<dict>")
        .ok_or(ManagedRuntimeError::Mount)?;
    let entity_end = text[mount_offset..]
        .find("</dict>")
        .map(|offset| mount_offset + offset + "</dict>".len())
        .ok_or(ManagedRuntimeError::Mount)?;
    let entity = &text[entity_start..entity_end];
    let device = plist_string(entity, "dev-entry")?;
    let mount = plist_string(entity, "mount-point")?;
    if !device.starts_with("/dev/disk") || !Path::new(&mount).is_absolute() {
        return Err(ManagedRuntimeError::Mount);
    }
    Ok((device, PathBuf::from(mount)))
}

fn plist_string(text: &str, key: &str) -> Result<String, ManagedRuntimeError> {
    let marker = format!("<key>{key}</key>");
    let (_, tail) = text.split_once(&marker).ok_or(ManagedRuntimeError::Mount)?;
    if tail.contains(&marker) {
        return Err(ManagedRuntimeError::Mount);
    }
    let (_, tail) = tail
        .split_once("<string>")
        .ok_or(ManagedRuntimeError::Mount)?;
    let (raw, _) = tail
        .split_once("</string>")
        .ok_or(ManagedRuntimeError::Mount)?;
    if raw.len() > 4096 || raw.contains('&') || raw.contains('<') || raw.contains('>') {
        return Err(ManagedRuntimeError::Mount);
    }
    Ok(raw.to_owned())
}

fn inspect_mount(device: &str, mount_point: &Path) -> Result<AttachedMount, ManagedRuntimeError> {
    let device_c = CString::new(device).map_err(|_| ManagedRuntimeError::Mount)?;
    // SAFETY: fixed NUL-terminated path and initialized stat buffer.
    let mut device_stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::stat(device_c.as_ptr(), &mut device_stat) } != 0 {
        return Err(ManagedRuntimeError::Mount);
    }
    let readonly = mount_is_readonly(mount_point)?;
    Ok(AttachedMount {
        mount_point: mount_point.to_owned(),
        device: device.to_owned(),
        device_number: device_stat.st_rdev as u64,
        readonly,
    })
}

fn mount_is_readonly(path: &Path) -> Result<bool, ManagedRuntimeError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| ManagedRuntimeError::Mount)?;
    // SAFETY: valid C path and initialized statfs buffer.
    let mut status = unsafe { std::mem::zeroed::<libc::statfs>() };
    if unsafe { libc::statfs(path.as_ptr(), &mut status) } != 0 {
        return Err(ManagedRuntimeError::Mount);
    }
    Ok(status.f_flags & (libc::MNT_RDONLY as u32) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, sync::Mutex};

    struct FakeMount {
        root: PathBuf,
        readonly: bool,
        device_offset: u64,
        detached: Arc<Mutex<Vec<String>>>,
    }
    impl MountBackend for FakeMount {
        fn attach(&self, _image: &File) -> Result<AttachedMount, ManagedRuntimeError> {
            Ok(AttachedMount {
                mount_point: self.root.clone(),
                device: "/dev/disk-fixture".into(),
                device_number: fs::metadata(&self.root).unwrap().dev() + self.device_offset,
                readonly: self.readonly,
            })
        }
        fn detach(&self, device: &str) {
            self.detached.lock().unwrap().push(device.to_owned());
        }
        fn validate_mount(&self, mount_point: &Path, device_number: u64) -> bool {
            self.readonly
                && fs::metadata(mount_point)
                    .ok()
                    .is_some_and(|metadata| metadata.dev() == device_number)
        }
    }

    fn put(path: &Path, bytes: &[u8], executable: bool) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if fs::symlink_metadata(path).is_ok() {
            fs::remove_file(path).unwrap();
        }
        fs::write(path, bytes).unwrap();
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
        )
        .unwrap();
    }

    fn fixture_lock(root: &Path, image: &[u8]) -> Vec<u8> {
        let files = vec![
            RuntimeTreeFileV2 {
                path: "runtime/bin/python3.11".into(),
                size: 6,
                sha256: sha256_bytes(b"python"),
                executable: true,
            },
            RuntimeTreeFileV2 {
                path: "runtime/lib/module.py".into(),
                size: 6,
                sha256: sha256_bytes(b"module"),
                executable: false,
            },
        ];
        put(&root.join("runtime/bin/python3.11"), b"python", true);
        put(&root.join("runtime/lib/module.py"), b"module", false);
        let tree = RuntimeTreeManifestV2 {
            schema_version: 1,
            files,
        };
        let tree_hash = sha256_bytes(&serde_json::to_vec(&tree).unwrap());
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        serde_json::to_vec(&serde_json::json!({
            "schema_version":2,
            "images":[{
                "id":IMAGE_ID,"version":"fixture","platform":"macos","architecture":arch,
                "image_filename":"runtime.dmg","image_size":image.len(),
                "image_sha256":sha256_bytes(image),"tree_manifest_sha256":tree_hash,
                "tree_manifest":tree,"bundled_resource":"runtime.dmg"
            }]
        }))
        .unwrap()
    }

    #[test]
    fn production_lock_is_valid_and_unavailable() {
        assert_eq!(
            select_image(EMBEDDED_LOCK).unwrap_err(),
            ManagedRuntimeError::Unavailable
        );
    }

    #[test]
    fn attach_argv_uses_fixed_inherited_fd_and_child_can_read_it() {
        assert_eq!(
            attach_arguments(),
            [
                "attach",
                "-plist",
                "-readonly",
                "-nobrowse",
                "-noautoopen",
                "/dev/fd/198",
            ]
        );
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("runtime.dmg");
        fs::write(&image_path, b"inherited image bytes").unwrap();
        let image = File::open(image_path).unwrap();
        let output = run_with_inherited_image_fd(
            Path::new("/bin/cat"),
            &[format!("/dev/fd/{CHILD_IMAGE_FD}")],
            &image,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"inherited image bytes");
        // The fixed duplicate belongs only to the child and must not remain
        // open in the parent after spawn/wait.
        assert_eq!(unsafe { libc::fcntl(CHILD_IMAGE_FD, libc::F_GETFD) }, -1);
    }

    #[test]
    fn package_tree_manifest_hash_replacement_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("mount");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture_lock(&mount, b"image")).unwrap();
        value["images"][0]["tree_manifest_sha256"] = serde_json::Value::String("0".repeat(64));
        assert_eq!(
            select_image(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            ManagedRuntimeError::Integrity
        );
    }

    #[test]
    fn fixture_mount_rejects_writable_extra_alias_hash_and_path_rename() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let mount = temp.path().join("mount");
        let image = b"fixture image";
        let lock = fixture_lock(&mount, image);
        let image_path = home.join(".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg");
        put(&image_path, image, false);
        let detached = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(FakeMount {
            root: mount.clone(),
            readonly: true,
            device_offset: 0,
            detached: detached.clone(),
        });
        let selected = select_image(&lock).unwrap();
        verify_tree(&mount, &selected.tree_manifest).unwrap();
        put(&mount.join("extra.py"), b"extra", false);
        assert_eq!(
            verify_tree(&mount, &selected.tree_manifest).unwrap_err(),
            ManagedRuntimeError::Integrity
        );
        fs::remove_file(mount.join("extra.py")).unwrap();
        put(&mount.join("runtime/lib/module.py"), b"xxxxxx", false);
        assert_eq!(
            verify_tree(&mount, &selected.tree_manifest).unwrap_err(),
            ManagedRuntimeError::Integrity
        );
        fs::remove_file(mount.join("runtime/lib/module.py")).unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("outside"),
            mount.join("runtime/lib/module.py"),
        )
        .unwrap();
        assert_eq!(
            verify_tree(&mount, &selected.tree_manifest).unwrap_err(),
            ManagedRuntimeError::Integrity
        );
        fs::remove_file(mount.join("runtime/lib/module.py")).unwrap();
        put(&mount.join("runtime/lib/module.py"), b"module", false);
        let lease = ManagedRuntimeLease::acquire_with(&lock, &home, backend).unwrap();
        lease.validate_before_spawn().unwrap();
        let replacement = image_path.with_extension("replacement");
        put(&replacement, image, false);
        fs::rename(&replacement, &image_path).unwrap();
        assert_eq!(
            lease.validate_before_spawn().unwrap_err(),
            ManagedRuntimeError::Integrity
        );
        drop(lease);
        assert_eq!(detached.lock().unwrap().as_slice(), &["/dev/disk-fixture"]);
        let writable = FakeMount {
            root: mount.clone(),
            readonly: false,
            device_offset: 0,
            detached: detached.clone(),
        };
        assert!(matches!(
            ManagedRuntimeLease::acquire_with(&lock, &home, Arc::new(writable)),
            Err(ManagedRuntimeError::Mount)
        ));
        let wrong_device = FakeMount {
            root: mount,
            readonly: true,
            device_offset: 1,
            detached,
        };
        assert!(matches!(
            ManagedRuntimeLease::acquire_with(&lock, &home, Arc::new(wrong_device)),
            Err(ManagedRuntimeError::Mount)
        ));
    }

    #[test]
    fn wrong_arch_and_plist_identity_are_rejected_and_detach_has_no_force() {
        let wrong = br#"{"schema_version":2,"images":[{"id":"python-e5-cat","version":"v","platform":"macos","architecture":"wrong","image_filename":"r.dmg","image_size":1,"image_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tree_manifest_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","tree_manifest":{"schema_version":1,"files":[]}}]}"#;
        assert_eq!(
            select_image(wrong).unwrap_err(),
            ManagedRuntimeError::Unavailable
        );
        assert!(parse_attach_plist(b"not plist").is_err());
        let valid = br#"<?xml version="1.0"?><plist><dict><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk9</string></dict><dict><key>dev-entry</key><string>/dev/disk9s1</string><key>mount-point</key><string>/Volumes/Runtime</string></dict></array></dict></plist>"#;
        assert_eq!(parse_attach_plist(valid).unwrap().0, "/dev/disk9s1");
        let ambiguous = br#"<?xml version="1.0"?><plist><dict><key>system-entities</key><array><dict><key>dev-entry</key><string>/dev/disk9s1</string><key>mount-point</key><string>/Volumes/One</string></dict><dict><key>dev-entry</key><string>/dev/disk9s2</string><key>mount-point</key><string>/Volumes/Two</string></dict></array></dict></plist>"#;
        assert!(parse_attach_plist(ambiguous).is_err());
        let source = include_str!("managed_runtime.rs");
        assert!(source.contains(".args([\"detach\", device])"));
        assert!(!source.contains("\"-force\""));
    }
}
