//! Package-trusted, read-only Python runtime image.
//!
//! The production lock is embedded in this binary.  A receipt next to a
//! downloaded image is repair/UI metadata only and never participates in the
//! trust decision.  The shipped lock deliberately has no image until a signed
//! release DMG and its complete tree manifest are produced.

#[cfg(any(unix, test))]
use serde::{Deserialize, Serialize};
#[cfg(any(unix, test))]
use sha2::{Digest, Sha256};
#[cfg(any(unix, test))]
use std::{collections::BTreeSet, path::Component};
#[cfg(unix)]
use std::{
    fs::{File, OpenOptions},
    os::unix::{fs::FileExt, fs::MetadataExt, fs::OpenOptionsExt},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(target_os = "macos")]
use std::{
    ffi::{CString, OsStr},
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    process::{Command, Output, Stdio},
    sync::Mutex,
};

#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;

#[cfg(any(unix, test))]
const EMBEDDED_LOCK: &[u8] = include_bytes!(
    "../../../apps/macos/Sources/PolarisDesktop/Resources/readonly-runtime-images-v2.json"
);
#[cfg(any(unix, test))]
const IMAGE_ID: &str = "python-e5-cat";
#[cfg(any(unix, test))]
const MAX_LOCK_BYTES: usize = 16 * 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_FILES: usize = 100_000;
#[cfg(any(unix, test))]
const MAX_TREE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MAX_PLIST_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const CHILD_IMAGE_FD: libc::c_int = 198;
#[cfg(target_os = "macos")]
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

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeImageLockV2 {
    schema_version: u32,
    images: Vec<RuntimeImageV2>,
}

#[cfg(any(unix, test))]
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

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTreeManifestV2 {
    schema_version: u32,
    files: Vec<RuntimeTreeFileV2>,
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTreeFileV2 {
    path: String,
    size: u64,
    sha256: String,
    executable: bool,
}

#[cfg(unix)]
#[derive(Debug)]
struct AttachedMount {
    mount_point: PathBuf,
    device: String,
    device_number: u64,
    readonly: bool,
}

#[cfg(unix)]
trait MountBackend: Send + Sync {
    fn attach(&self, image: &File) -> Result<AttachedMount, ManagedRuntimeError>;
    fn validate_mount(&self, mount_point: &Path, device_number: u64) -> bool;
    fn detach(&self, device: &str);
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct HdiutilBackend;

#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug)]
struct UnsupportedMountBackend;

#[cfg(target_os = "macos")]
struct ReservedChildFd(libc::c_int);

#[cfg(target_os = "macos")]
impl Drop for ReservedChildFd {
    fn drop(&mut self) {
        // SAFETY: this descriptor is created and exclusively owned below.
        unsafe { libc::close(self.0) };
    }
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(all(unix, not(target_os = "macos")))]
impl MountBackend for UnsupportedMountBackend {
    fn attach(&self, _image: &File) -> Result<AttachedMount, ManagedRuntimeError> {
        Err(ManagedRuntimeError::Unavailable)
    }

    fn validate_mount(&self, _mount_point: &Path, _device_number: u64) -> bool {
        false
    }

    fn detach(&self, _device: &str) {}
}

struct RuntimeInner {
    #[cfg(unix)]
    image_path: PathBuf,
    #[cfg(unix)]
    image_identity: (u64, u64),
    #[cfg(unix)]
    image_size: u64,
    #[cfg(unix)]
    image_sha256: String,
    #[cfg(unix)]
    image: File,
    #[cfg(unix)]
    mount_root_identity: (u64, u64),
    #[cfg(unix)]
    mount_root: File,
    #[cfg(unix)]
    python_identity: (u64, u64),
    #[cfg(unix)]
    python: File,
    mount_point: PathBuf,
    #[cfg(unix)]
    device: String,
    #[cfg(unix)]
    device_number: u64,
    #[cfg(unix)]
    manifest: RuntimeTreeManifestV2,
    #[cfg(unix)]
    backend: Arc<dyn MountBackend>,
}

#[cfg(unix)]
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
    #[cfg(unix)]
    pub fn acquire(home: &Path) -> Result<Self, ManagedRuntimeError> {
        #[cfg(target_os = "macos")]
        let backend: Arc<dyn MountBackend> = Arc::new(HdiutilBackend);
        #[cfg(not(target_os = "macos"))]
        let backend: Arc<dyn MountBackend> = Arc::new(UnsupportedMountBackend);
        Self::acquire_with(EMBEDDED_LOCK, home, backend)
    }

    #[cfg(not(unix))]
    pub fn acquire(_home: &Path) -> Result<Self, ManagedRuntimeError> {
        Err(ManagedRuntimeError::Unavailable)
    }

    #[cfg(unix)]
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
        let mount_root_identity = (root_metadata.dev(), root_metadata.ino());
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
        let python_identity = (python_metadata.dev(), python_metadata.ino());
        let lease = Self(Arc::new(RuntimeInner {
            image_path,
            image_identity,
            image_size: image.image_size,
            image_sha256: image.image_sha256,
            image: image_file,
            mount_root_identity,
            mount_root,
            python_identity,
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

    #[cfg(not(unix))]
    pub fn validate_before_spawn(&self) -> Result<(), ManagedRuntimeError> {
        Err(ManagedRuntimeError::Unavailable)
    }

    #[cfg(unix)]
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
        if image.len() != self.0.image_size || sha256_reader(&self.0.image)? != self.0.image_sha256
        {
            return Err(ManagedRuntimeError::Integrity);
        }
        let root = self
            .0
            .mount_root
            .metadata()
            .map_err(|_| ManagedRuntimeError::Mount)?;
        if (root.dev(), root.ino()) != self.0.mount_root_identity
            || root.dev() != self.0.device_number
            || !path_has_directory_identity(&self.0.mount_point, self.0.mount_root_identity)
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
        if (python.dev(), python.ino()) != self.0.python_identity
            || python.dev() != self.0.device_number
            || python.mode() & 0o111 == 0
            || check_path_identity(&self.python_path(), self.0.python_identity).is_err()
        {
            return Err(ManagedRuntimeError::Integrity);
        }
        verify_tree(&self.0.mount_point, &self.0.manifest)
    }
}

#[cfg(any(unix, test))]
fn select_image(bytes: &[u8]) -> Result<RuntimeImageV2, ManagedRuntimeError> {
    if bytes.len() > MAX_LOCK_BYTES {
        return Err(ManagedRuntimeError::Integrity);
    }
    let lock: RuntimeImageLockV2 =
        serde_json::from_slice(bytes).map_err(|_| ManagedRuntimeError::Integrity)?;
    if lock.schema_version != 2 {
        return Err(ManagedRuntimeError::Integrity);
    }
    let (platform, architecture) = current_platform_architecture()?;
    let mut matches = lock.images.into_iter().filter(|entry| {
        entry.id == IMAGE_ID && entry.platform == platform && entry.architecture == architecture
    });
    let entry = matches.next().ok_or(ManagedRuntimeError::Unavailable)?;
    if matches.next().is_some() {
        return Err(ManagedRuntimeError::Unavailable);
    }
    if !valid_component(&entry.version)
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

#[cfg(any(unix, test))]
fn current_platform_architecture() -> Result<(&'static str, &'static str), ManagedRuntimeError> {
    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return Err(ManagedRuntimeError::Unavailable);
    };
    let architecture = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return Err(ManagedRuntimeError::Unavailable);
    };
    Ok((platform, architecture))
}

#[cfg(any(unix, test))]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
fn path_has_directory_identity(path: &Path, expected: (u64, u64)) -> bool {
    std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.file_type().is_dir()
                && (metadata.dev(), metadata.ino()) == expected
        })
}

#[cfg(unix)]
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

#[cfg(unix)]
fn sha256_path(path: &Path) -> Result<String, ManagedRuntimeError> {
    sha256_reader(&open_regular_nofollow(path, false)?)
}

#[cfg(any(unix, test))]
fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(any(unix, test))]
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(any(unix, test))]
fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !matches!(value, "." | "..")
        && !value.bytes().any(|byte| byte == b'/' || byte == 0)
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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
mod portable_tests {
    use super::*;

    #[test]
    fn production_lock_is_valid_and_unavailable() {
        assert_eq!(
            select_image(EMBEDDED_LOCK).unwrap_err(),
            ManagedRuntimeError::Unavailable
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_unavailable_acquire_preserves_existing_and_missing_home() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("existing.txt");
        std::fs::write(&marker, b"keep runtime state").unwrap();
        let missing = root.path().join("missing-home");

        for home in [root.path(), missing.as_path()] {
            assert_eq!(
                ManagedRuntimeLease::acquire(home).err(),
                Some(ManagedRuntimeError::Unavailable)
            );
        }

        assert_eq!(std::fs::read(&marker).unwrap(), b"keep runtime state");
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_unavailable_pre_spawn_preserves_path_and_clone_contract() {
        fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
        assert_send_sync_clone::<ManagedRuntimeLease>();

        let root = tempfile::tempdir().unwrap();
        let mount_point = root.path().join("missing-mount");
        let lease = ManagedRuntimeLease(Arc::new(RuntimeInner {
            mount_point: mount_point.clone(),
        }));
        let cloned = lease.clone();
        assert_eq!(
            cloned.python_path(),
            mount_point.join("runtime/bin/python3.11")
        );
        for candidate in [&lease, &cloned] {
            assert_eq!(
                candidate.validate_before_spawn(),
                Err(ManagedRuntimeError::Unavailable)
            );
        }
        assert!(!mount_point.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[cfg(all(test, unix))]
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
        let (platform, arch) = current_platform_architecture().unwrap();
        serde_json::to_vec(&serde_json::json!({
            "schema_version":2,
            "images":[{
                "id":IMAGE_ID,"version":"fixture","platform":platform,"architecture":arch,
                "image_filename":"runtime.dmg","image_size":image.len(),
                "image_sha256":sha256_bytes(image),"tree_manifest_sha256":tree_hash,
                "tree_manifest":tree,"bundled_resource":"runtime.dmg"
            }]
        }))
        .unwrap()
    }

    struct NoSpawnBackend;

    impl MountBackend for NoSpawnBackend {
        fn attach(&self, _image: &File) -> Result<AttachedMount, ManagedRuntimeError> {
            panic!("an unavailable lock must not invoke a mount helper")
        }

        fn validate_mount(&self, _mount_point: &Path, _device_number: u64) -> bool {
            false
        }

        fn detach(&self, _device: &str) {}
    }

    #[test]
    fn empty_lock_is_unavailable_before_any_helper_can_spawn() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            ManagedRuntimeLease::acquire_with(
                br#"{"schema_version":2,"images":[]}"#,
                temp.path(),
                Arc::new(NoSpawnBackend),
            )
            .err(),
            Some(ManagedRuntimeError::Unavailable)
        );
    }

    #[cfg(target_os = "macos")]
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
    fn wrong_platform_image_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture_lock(temp.path(), b"image")).unwrap();
        let other_platform = if cfg!(target_os = "linux") {
            "macos"
        } else {
            "linux"
        };
        value["images"][0]["platform"] = serde_json::Value::String(other_platform.into());
        assert_eq!(
            select_image(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            ManagedRuntimeError::Unavailable
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
    fn duplicate_current_platform_image_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture_lock(temp.path(), b"image")).unwrap();
        let duplicate = value["images"][0].clone();
        value["images"].as_array_mut().unwrap().push(duplicate);
        assert_eq!(
            select_image(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            ManagedRuntimeError::Unavailable
        );
    }

    #[test]
    fn pre_spawn_rechecks_held_image_mount_python_and_tree_identities() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("image-home");
        let mount = temp.path().join("image-mount");
        let image = b"fixture image";
        let lock = fixture_lock(&mount, image);
        let image_path = home.join(".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg");
        put(&image_path, image, false);
        let backend = Arc::new(FakeMount {
            root: mount.clone(),
            readonly: true,
            device_offset: 0,
            detached: Arc::new(Mutex::new(Vec::new())),
        });
        let lease = ManagedRuntimeLease::acquire_with(&lock, &home, backend).unwrap();
        fs::set_permissions(&image_path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&image_path, b"tampered image").unwrap();
        assert_eq!(
            lease.validate_before_spawn().unwrap_err(),
            ManagedRuntimeError::Integrity
        );

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("python-home");
        let mount = temp.path().join("python-mount");
        let lock = fixture_lock(&mount, image);
        let image_path = home.join(".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg");
        put(&image_path, image, false);
        let lease = ManagedRuntimeLease::acquire_with(
            &lock,
            &home,
            Arc::new(FakeMount {
                root: mount.clone(),
                readonly: true,
                device_offset: 0,
                detached: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .unwrap();
        let python = mount.join("runtime/bin/python3.11");
        let replacement = python.with_extension("replacement");
        put(&replacement, b"python", true);
        fs::rename(replacement, python).unwrap();
        assert_eq!(
            lease.validate_before_spawn().unwrap_err(),
            ManagedRuntimeError::Integrity
        );

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("mount-home");
        let mount = temp.path().join("mount-root");
        let replacement_mount = temp.path().join("replacement-root");
        let lock = fixture_lock(&mount, image);
        fixture_lock(&replacement_mount, image);
        let image_path = home.join(".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg");
        put(&image_path, image, false);
        let lease = ManagedRuntimeLease::acquire_with(
            &lock,
            &home,
            Arc::new(FakeMount {
                root: mount.clone(),
                readonly: true,
                device_offset: 0,
                detached: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .unwrap();
        fs::rename(&mount, temp.path().join("old-root")).unwrap();
        fs::rename(&replacement_mount, &mount).unwrap();
        assert_eq!(
            lease.validate_before_spawn().unwrap_err(),
            ManagedRuntimeError::Mount
        );

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("tree-home");
        let mount = temp.path().join("tree-mount");
        let lock = fixture_lock(&mount, image);
        let image_path = home.join(".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg");
        put(&image_path, image, false);
        let lease = ManagedRuntimeLease::acquire_with(
            &lock,
            &home,
            Arc::new(FakeMount {
                root: mount.clone(),
                readonly: true,
                device_offset: 0,
                detached: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .unwrap();
        let module = mount.join("runtime/lib/module.py");
        fs::set_permissions(&module, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(module, b"tamper").unwrap();
        assert_eq!(
            lease.validate_before_spawn().unwrap_err(),
            ManagedRuntimeError::Integrity
        );
    }

    #[cfg(target_os = "macos")]
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
