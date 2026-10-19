//! C08/C10 shared, local Keychain credential binding.
//!
//! Durable values in this module deliberately contain only hashes and file
//! identities. Persistent references and password bytes remain in an owner
//! instance and never implement `Debug`, `Display`, or serialization.

use std::fmt;
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(all(test, unix))]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use ring::digest;
#[cfg(target_os = "macos")]
use ring::hmac;
#[cfg(unix)]
use ring::rand;
#[cfg(unix)]
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const NAMESPACE: &str = "macos-keychain.internet-password.v1";
const FINGERPRINT_KEY_BYTES: usize = 32;
#[cfg(target_os = "macos")]
const MAX_CANDIDATES: usize = 64;
#[cfg(target_os = "macos")]
const MAX_ATTRIBUTE_BYTES: usize = 128 * 1024;
#[cfg(unix)]
const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
#[cfg(unix)]
const MAX_CHILD_OUTPUT_BYTES: u64 = 64 * 1024;
pub const MAX_GIT_CREDENTIAL_VALUE_BYTES: usize = 128 * 1024;
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum GitCredentialErrorV1 {
    #[error("Git credential binding is unavailable on this platform")]
    UnavailablePlatform,
    #[error("invalid canonical HTTPS remote")]
    InvalidRemote,
    #[error("invalid credential binding")]
    InvalidBinding,
    #[error("no matching Keychain credential is available")]
    Missing,
    #[error("multiple Keychain credentials require explicit selection")]
    Ambiguous,
    #[error("the selected Keychain credential changed")]
    BindingChanged,
    #[error("credential owner filesystem protection failed")]
    Protection,
    #[error("Keychain access failed")]
    Keychain,
    #[error("I/O error")]
    Io(#[from] std::io::Error),
}

/// A canonical transport locator. Its Debug/Display form is intentionally
/// redacted; callers must opt in to the transport URL at the spawn boundary.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "CanonicalHttpsRemoteWireV1",
    into = "CanonicalHttpsRemoteWireV1"
)]
pub struct CanonicalHttpsRemoteV1 {
    host: String,
    port: u16,
    path: String,
}

impl CanonicalHttpsRemoteV1 {
    pub fn parse(input: &str) -> Result<Self, GitCredentialErrorV1> {
        if !input.starts_with("https://")
            || input
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || input.contains(['@', '?', '#'])
        {
            return Err(GitCredentialErrorV1::InvalidRemote);
        }
        let rest = &input[8..];
        let slash = rest.find('/').ok_or(GitCredentialErrorV1::InvalidRemote)?;
        let (authority, path) = rest.split_at(slash);
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or(GitCredentialErrorV1::InvalidRemote)?;
        if host.is_empty()
            || host.contains(':')
            || !host.is_ascii()
            || host != host.to_ascii_lowercase()
            || host.split('.').any(|label| {
                label.is_empty()
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return Err(GitCredentialErrorV1::InvalidRemote);
        }
        let port: u16 = port
            .parse()
            .map_err(|_| GitCredentialErrorV1::InvalidRemote)?;
        if port == 0
            || !path.starts_with('/')
            || path.contains("//")
            || path.contains('%')
            || path.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err(GitCredentialErrorV1::InvalidRemote);
        }
        let remote = Self {
            host: host.into(),
            port,
            path: path.into(),
        };
        if remote.transport_url() != input {
            return Err(GitCredentialErrorV1::InvalidRemote);
        }
        Ok(remote)
    }

    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn transport_url(&self) -> String {
        format!("https://{}:{}{}", self.host, self.port, self.path)
    }
    pub fn redacted_label(&self) -> String {
        format!(
            "https://host-{}:{}",
            &hex(digest::digest(&digest::SHA256, self.host.as_bytes()).as_ref())[..12],
            self.port
        )
    }
    fn validate(&self) -> Result<(), GitCredentialErrorV1> {
        if Self::parse(&self.transport_url())? == *self {
            Ok(())
        } else {
            Err(GitCredentialErrorV1::InvalidRemote)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalHttpsRemoteWireV1 {
    host: String,
    port: u16,
    path: String,
}
impl TryFrom<CanonicalHttpsRemoteWireV1> for CanonicalHttpsRemoteV1 {
    type Error = GitCredentialErrorV1;
    fn try_from(value: CanonicalHttpsRemoteWireV1) -> Result<Self, Self::Error> {
        Self::parse(&format!(
            "https://{}:{}{}",
            value.host, value.port, value.path
        ))
    }
}
impl From<CanonicalHttpsRemoteV1> for CanonicalHttpsRemoteWireV1 {
    fn from(value: CanonicalHttpsRemoteV1) -> Self {
        Self {
            host: value.host,
            port: value.port,
            path: value.path,
        }
    }
}

impl fmt::Debug for CanonicalHttpsRemoteV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted_label())
    }
}
impl fmt::Display for CanonicalHttpsRemoteV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted_label())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableIdentityV1 {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub owner_uid: u32,
    pub mode: u32,
    pub sha256: String,
}

impl ExecutableIdentityV1 {
    pub fn validate_structure(&self) -> Result<(), GitCredentialErrorV1> {
        if !self.path.is_absolute()
            || self.path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || self.device == 0
            || self.inode == 0
            || self.mode & 0o022 != 0
            || self.mode & 0o111 == 0
            || !valid_hex32(&self.sha256)
        {
            return Err(GitCredentialErrorV1::InvalidBinding);
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn inspect(path: &Path) -> Result<Self, GitCredentialErrorV1> {
        let canonical = path
            .canonicalize()
            .map_err(|_| GitCredentialErrorV1::Protection)?;
        if canonical != path {
            return Err(GitCredentialErrorV1::Protection);
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&canonical)?;
        let before = file.metadata()?;
        let euid = unsafe { libc::geteuid() };
        if !before.is_file()
            || (before.uid() != euid && before.uid() != 0)
            || before.nlink() != 1
            || before.mode() & 0o022 != 0
            || before.mode() & 0o111 == 0
            || before.len() > MAX_EXECUTABLE_BYTES
        {
            return Err(GitCredentialErrorV1::Protection);
        }
        let reopened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&canonical)?;
        let reopened_metadata = reopened.metadata()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) != (
            reopened_metadata.dev(),
            reopened_metadata.ino(),
            reopened_metadata.len(),
            reopened_metadata.mtime(),
            reopened_metadata.mtime_nsec(),
        ) {
            return Err(GitCredentialErrorV1::Protection);
        }
        let mut reader = file;
        let mut hasher = digest::Context::new(&digest::SHA256);
        let mut block = [0_u8; 64 * 1024];
        loop {
            let count = reader.read(&mut block)?;
            if count == 0 {
                break;
            }
            hasher.update(&block[..count]);
        }
        let after = reader.metadata()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
        ) {
            return Err(GitCredentialErrorV1::Protection);
        }
        Ok(Self {
            path: canonical,
            device: before.dev(),
            inode: before.ino(),
            owner_uid: before.uid(),
            mode: before.mode() & 0o7777,
            sha256: hex(hasher.finish().as_ref()),
        })
    }

    #[cfg(not(unix))]
    pub fn inspect(_path: &Path) -> Result<Self, GitCredentialErrorV1> {
        Err(GitCredentialErrorV1::UnavailablePlatform)
    }

    pub fn revalidate(&self) -> Result<(), GitCredentialErrorV1> {
        if Self::inspect(&self.path)? == *self {
            Ok(())
        } else {
            Err(GitCredentialErrorV1::BindingChanged)
        }
    }
}

impl fmt::Debug for ExecutableIdentityV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutableIdentityV1")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationTypeV1 {
    Default,
    HttpBasic,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "state", rename_all = "snake_case")]
pub enum OptionalAttributeHashV1 {
    Absent,
    Present { sha256: String },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitCredentialBindingV1 {
    pub namespace: String,
    pub credential_item_id: String,
    pub credential_item_version: String,
    pub remote: CanonicalHttpsRemoteV1,
    pub account: String,
    pub authentication_type: AuthenticationTypeV1,
    pub security_domain: OptionalAttributeHashV1,
    pub access_group: OptionalAttributeHashV1,
    pub synchronizable: bool,
    pub generation: u64,
    pub git: ExecutableIdentityV1,
    pub askpass: ExecutableIdentityV1,
}

/// A redacted, explicit selection token for an otherwise identical Keychain
/// candidate. It is derived from the persistent-reference digest only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialCandidateChoiceV1 {
    pub item_id_prefix: String,
}

impl fmt::Debug for CredentialCandidateChoiceV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialCandidateChoiceV1")
            .field("item_id_prefix", &self.item_id_prefix)
            .finish()
    }
}

impl GitCredentialBindingV1 {
    pub fn validate(&self) -> Result<(), GitCredentialErrorV1> {
        if self.namespace != NAMESPACE
            || self.generation == 0
            || self.synchronizable
            || !valid_hex32(&self.credential_item_id)
            || !valid_hex32(&self.credential_item_version)
            || self.account.is_empty()
            || self.account.len() > 256
        {
            return Err(GitCredentialErrorV1::InvalidBinding);
        }
        self.remote.validate()?;
        for value in [&self.security_domain, &self.access_group] {
            if let OptionalAttributeHashV1::Present { sha256 } = value
                && !valid_hex32(sha256)
            {
                return Err(GitCredentialErrorV1::InvalidBinding);
            }
        }
        self.git.validate_structure()?;
        self.askpass.validate_structure()?;
        // This is deliberately structural: deserializers and C08 inventory
        // scans must not touch Keychain or executable paths. The owner does
        // the live binary and Keychain revalidation immediately before use.
        if self.remote.transport_url().is_empty() {
            return Err(GitCredentialErrorV1::InvalidBinding);
        }
        Ok(())
    }
    pub fn matches_remote(&self, remote: &CanonicalHttpsRemoteV1) -> bool {
        &self.remote == remote
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl fmt::Debug for GitCredentialBindingV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitCredentialBindingV1")
            .field("remote", &self.remote)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// In-memory-only material. Consumers can hand it to a child-spawn boundary
/// but cannot serialize or format it.
pub struct GitCredentialMaterialV1 {
    username: Zeroizing<Vec<u8>>,
    password: Zeroizing<Vec<u8>>,
}
impl GitCredentialMaterialV1 {
    pub fn username(&self) -> &[u8] {
        &self.username
    }
    pub fn password(&self) -> &[u8] {
        &self.password
    }
}

/// Whether Git could have started. Remote owners must never interpret a
/// failure after spawn as a zero-send result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitChildFailurePhaseV1 {
    BeforeSpawn,
    AfterSpawn,
}

/// Redacted child failure; underlying errors are retained only for the legacy
/// credential-owner adapter, never formatted as paths or child output.
pub struct GitChildFailureV1 {
    phase: GitChildFailurePhaseV1,
    error: GitCredentialErrorV1,
}
impl GitChildFailureV1 {
    pub fn phase(&self) -> GitChildFailurePhaseV1 {
        self.phase
    }
    fn before(error: GitCredentialErrorV1) -> Self {
        Self {
            phase: GitChildFailurePhaseV1::BeforeSpawn,
            error,
        }
    }
    #[cfg(unix)]
    fn after(error: GitCredentialErrorV1) -> Self {
        Self {
            phase: GitChildFailurePhaseV1::AfterSpawn,
            error,
        }
    }
}
impl fmt::Debug for GitChildFailureV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitChildFailureV1")
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}
impl fmt::Display for GitChildFailureV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.phase {
            GitChildFailurePhaseV1::BeforeSpawn => "git_child_failed_before_spawn",
            GitChildFailurePhaseV1::AfterSpawn => "git_child_failed_after_spawn",
        })
    }
}
impl std::error::Error for GitChildFailureV1 {}

/// In-memory child data. Consumers parse bounded output at their domain
/// boundary; raw stdout/stderr must not be displayed or serialized.
pub struct GitChildOutputV1 {
    stdout: Zeroizing<Vec<u8>>,
    status: std::process::ExitStatus,
}
impl GitChildOutputV1 {
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }
    pub fn success(&self) -> bool {
        self.status.success()
    }
    pub fn exit_code(&self) -> Option<i32> {
        self.status.code()
    }
}

/// Git's SHA-1 blob identity for already bounded, pinned shared-file bytes.
/// This is not the credential fingerprint or the SHA-256 payload commitment.
pub fn git_blob_oid_from_bytes(bytes: &[u8]) -> String {
    let mut hash = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
    hash.update(format!("blob {}\0", bytes.len()).as_bytes());
    hash.update(bytes);
    hex(hash.finish().as_ref())
}

/// Bound local Git only. The repository owner must pin and validate repository
/// controls/config/ancestors before every call; this runner is not admission.
pub fn run_bound_local_git(
    git: &ExecutableIdentityV1,
    args: &[std::ffi::OsString],
    index: &Path,
) -> Result<GitChildOutputV1, GitChildFailureV1> {
    run_git_child(git, None, None, args, Some(index), CHILD_TIMEOUT)
}

pub struct GitCredentialOwnerV1 {
    binding_key_path: PathBuf,
    git: ExecutableIdentityV1,
    askpass: ExecutableIdentityV1,
}

impl GitCredentialOwnerV1 {
    pub fn new(home: &Path, git: &Path, askpass: &Path) -> Result<Self, GitCredentialErrorV1> {
        let git = ExecutableIdentityV1::inspect(git)?;
        let askpass = ExecutableIdentityV1::inspect(askpass)?;
        let binding_key_path = home.join(".polaris/git-credential-bindings-v1/fingerprint.key");
        crate::protection::protect_path(&binding_key_path)
            .map_err(|_| GitCredentialErrorV1::Protection)?;
        Ok(Self {
            binding_key_path,
            git,
            askpass,
        })
    }

    pub fn git_identity(&self) -> &ExecutableIdentityV1 {
        &self.git
    }

    pub fn revalidate_binaries(&self) -> Result<(), GitCredentialErrorV1> {
        self.git.revalidate()?;
        self.askpass.revalidate()
    }

    pub fn binding_key(
        &self,
    ) -> Result<Zeroizing<[u8; FINGERPRINT_KEY_BYTES]>, GitCredentialErrorV1> {
        load_or_create_binding_key(&self.binding_key_path)
    }

    /// Executes only the bound absolute Git binary. Credential bytes travel
    /// through two anonymous read-only askpass descriptors, never argv/env.
    pub fn run_bound_git(
        &self,
        args: &[std::ffi::OsString],
        material: &GitCredentialMaterialV1,
    ) -> Result<(), GitCredentialErrorV1> {
        self.run_bound_git_inner(args, material, CHILD_TIMEOUT)
    }

    pub fn run_bound_git_output(
        &self,
        args: &[std::ffi::OsString],
        material: &GitCredentialMaterialV1,
    ) -> Result<GitChildOutputV1, GitChildFailureV1> {
        run_git_child(
            &self.git,
            Some(&self.askpass),
            Some(material),
            args,
            None,
            CHILD_TIMEOUT,
        )
    }

    /// The repository owner supplies its already pinned dedicated index.
    pub fn run_bound_git_output_at_index(
        &self,
        args: &[std::ffi::OsString],
        material: &GitCredentialMaterialV1,
        index: &Path,
    ) -> Result<GitChildOutputV1, GitChildFailureV1> {
        run_git_child(
            &self.git,
            Some(&self.askpass),
            Some(material),
            args,
            Some(index),
            CHILD_TIMEOUT,
        )
    }

    #[cfg(test)]
    pub(crate) fn run_bound_git_with_timeout(
        &self,
        args: &[std::ffi::OsString],
        material: &GitCredentialMaterialV1,
        timeout: Duration,
    ) -> Result<(), GitCredentialErrorV1> {
        self.run_bound_git_inner(args, material, timeout)
    }

    fn run_bound_git_inner(
        &self,
        args: &[std::ffi::OsString],
        material: &GitCredentialMaterialV1,
        timeout: Duration,
    ) -> Result<(), GitCredentialErrorV1> {
        let output = run_git_child(
            &self.git,
            Some(&self.askpass),
            Some(material),
            args,
            None,
            timeout,
        )
        .map_err(|failure| failure.error)?;
        if output.success() {
            Ok(())
        } else {
            Err(GitCredentialErrorV1::Protection)
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn read_bound_material(
        &self,
        _binding: &GitCredentialBindingV1,
    ) -> Result<GitCredentialMaterialV1, GitCredentialErrorV1> {
        Err(GitCredentialErrorV1::UnavailablePlatform)
    }

    /// Re-read a durable binding without creating a Keychain item or repairing
    /// state. C08 uses this after restart to classify a stale binding.
    #[cfg(not(target_os = "macos"))]
    pub fn revalidate_binding(
        &self,
        _binding: &GitCredentialBindingV1,
    ) -> Result<(), GitCredentialErrorV1> {
        Err(GitCredentialErrorV1::UnavailablePlatform)
    }

    /// Non-macOS never falls back to a file-based Keychain.
    #[cfg(not(target_os = "macos"))]
    pub fn select(
        &self,
        _remote: &CanonicalHttpsRemoteV1,
        _account: &str,
        _generation: u64,
        _choice: Option<&str>,
    ) -> Result<(GitCredentialBindingV1, GitCredentialMaterialV1), GitCredentialErrorV1> {
        Err(GitCredentialErrorV1::UnavailablePlatform)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn candidate_choices(
        &self,
        _remote: &CanonicalHttpsRemoteV1,
        _account: &str,
    ) -> Result<Vec<CredentialCandidateChoiceV1>, GitCredentialErrorV1> {
        Err(GitCredentialErrorV1::UnavailablePlatform)
    }
}

/// A fixed non-user HOME prevents libcurl's passwd-home .netrc fallback.
/// No file is opened as a credential source and no directory is created.
#[cfg(unix)]
struct NetworkHomeRootV1 {
    directory: File,
    metadata: std::fs::Metadata,
}
#[cfg(unix)]
impl NetworkHomeRootV1 {
    fn inspect() -> Result<Self, GitCredentialErrorV1> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open("/")?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != 0
            || metadata.mode() & 0o7022 != 0
            || metadata.nlink() == 0
        {
            return Err(GitCredentialErrorV1::Protection);
        }
        let result = Self {
            directory,
            metadata,
        };
        result.verify()?;
        Ok(result)
    }
    fn verify(&self) -> Result<(), GitCredentialErrorV1> {
        require_netrc_absent(&self.directory)?;
        let named = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open("/")?;
        let same = |a: &std::fs::Metadata, b: &std::fs::Metadata| {
            (
                a.dev(),
                a.ino(),
                a.uid(),
                a.mode(),
                a.nlink(),
                a.len(),
                a.mtime(),
                a.mtime_nsec(),
                a.ctime(),
                a.ctime_nsec(),
            ) == (
                b.dev(),
                b.ino(),
                b.uid(),
                b.mode(),
                b.nlink(),
                b.len(),
                b.mtime(),
                b.mtime_nsec(),
                b.ctime(),
                b.ctime_nsec(),
            )
        };
        if !same(&self.metadata, &self.directory.metadata()?)
            || !same(&self.metadata, &named.metadata()?)
        {
            return Err(GitCredentialErrorV1::Protection);
        }
        require_netrc_absent(&named)
    }
}
#[cfg(unix)]
fn require_netrc_absent(directory: &File) -> Result<(), GitCredentialErrorV1> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            c".netrc".as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Err(GitCredentialErrorV1::Protection);
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
        return Err(GitCredentialErrorV1::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn run_git_child(
    git: &ExecutableIdentityV1,
    askpass: Option<&ExecutableIdentityV1>,
    material: Option<&GitCredentialMaterialV1>,
    args: &[std::ffi::OsString],
    index: Option<&Path>,
    timeout: Duration,
) -> Result<GitChildOutputV1, GitChildFailureV1> {
    git.revalidate().map_err(GitChildFailureV1::before)?;
    if let Some(askpass) = askpass {
        askpass.revalidate().map_err(GitChildFailureV1::before)?;
    }
    if askpass.is_some() != material.is_some() {
        return Err(GitChildFailureV1::before(
            GitCredentialErrorV1::InvalidBinding,
        ));
    }
    if let Some(index) = index {
        use std::path::Component;
        if !index.is_absolute()
            || index.file_name().is_none()
            || index
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
            || index
                .as_os_str()
                .as_bytes()
                .windows(2)
                .any(|bytes| bytes == b"//")
            || index
                .as_os_str()
                .as_bytes()
                .split(|byte| *byte == b'/')
                .any(|part| part == b"." || part == b"..")
            || index.as_os_str().as_bytes().ends_with(b"/")
            || index.as_os_str().as_bytes().contains(&0)
        {
            return Err(GitChildFailureV1::before(GitCredentialErrorV1::Protection));
        }
    }
    if let Some(material) = material
        && ([material.username(), material.password()]
            .iter()
            .any(|value| {
                value.is_empty()
                    || value.len() > MAX_GIT_CREDENTIAL_VALUE_BYTES
                    || value
                        .iter()
                        .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
            })
            || args.iter().any(|arg| {
                let value = arg.as_os_str().as_bytes();
                value
                    .windows(material.username().len())
                    .any(|part| part == material.username())
                    || value
                        .windows(material.password().len())
                        .any(|part| part == material.password())
            }))
    {
        return Err(GitChildFailureV1::before(GitCredentialErrorV1::Protection));
    }
    let network_home = material
        .map(|_| NetworkHomeRootV1::inspect())
        .transpose()
        .map_err(GitChildFailureV1::before)?;
    let mut handoff = if material.is_some() {
        let (username_read, username_write) =
            anonymous_pipe().map_err(GitChildFailureV1::before)?;
        let (password_read, password_write) =
            anonymous_pipe().map_err(GitChildFailureV1::before)?;
        let username_child =
            duplicate_cloexec_fd(username_read.as_raw_fd()).map_err(GitChildFailureV1::before)?;
        let password_child =
            duplicate_cloexec_fd(password_read.as_raw_fd()).map_err(GitChildFailureV1::before)?;
        Some((
            username_read,
            username_write,
            password_read,
            password_write,
            username_child,
            password_child,
        ))
    } else {
        None
    };
    let descriptors = handoff
        .as_ref()
        .map(|(_, _, _, _, username, password)| (username.as_raw_fd(), password.as_raw_fd()));
    let mut command = Command::new(&git.path);
    command
        .args(args)
        .env_clear()
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(askpass) = askpass {
        command.env("HOME", "/");
        command
            .env("GIT_ASKPASS", &askpass.path)
            .env("POLARIS_GIT_ASKPASS_USERNAME_FD", "3")
            .env("POLARIS_GIT_ASKPASS_PASSWORD_FD", "4");
    } else {
        command.env("GIT_ASKPASS", "");
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    unsafe {
        command.pre_exec(move || {
            libc::umask(0o077);
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some((username, password)) = descriptors {
                if libc::dup2(username, 3) < 0 || libc::dup2(password, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in [3, 4] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
            }
            Ok(())
        });
    }
    if let Some(home) = &network_home {
        home.verify().map_err(GitChildFailureV1::before)?;
    }
    let mut child = command
        .spawn()
        .map_err(|_| GitChildFailureV1::before(GitCredentialErrorV1::Protection))?;
    let deadline = Instant::now() + timeout;
    if let Some((
        username_read,
        mut username_write,
        password_read,
        mut password_write,
        username_child,
        password_child,
    )) = handoff.take()
    {
        drop(username_child);
        drop(password_child);
        drop(username_read);
        drop(password_read);
        let material = material.expect("handoff only for validated material");
        if let Err(error) = write_pipe_until(&mut username_write, material.username(), deadline)
            .and_then(|_| write_pipe_until(&mut password_write, material.password(), deadline))
        {
            return fail_after_spawn(&mut child, error);
        }
    }
    let (mut stdout, mut stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        _ => {
            return fail_after_spawn(&mut child, GitCredentialErrorV1::Protection);
        }
    };
    if set_nonblocking(stdout.as_raw_fd()).is_err() || set_nonblocking(stderr.as_raw_fd()).is_err()
    {
        return fail_after_spawn(&mut child, GitCredentialErrorV1::Protection);
    }
    let mut stdout_body = Zeroizing::new(Vec::new());
    let mut stderr_body = Zeroizing::new(Vec::new());
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    let mut leader_exited = false;
    loop {
        match drain_child_output(&mut stdout, &mut stdout_body).and_then(|closed| {
            drain_child_output(&mut stderr, &mut stderr_body).map(|other| (closed, other))
        }) {
            Ok((left, right)) => {
                stdout_closed |= left;
                stderr_closed |= right;
            }
            Err(_) => {
                return fail_after_spawn(&mut child, GitCredentialErrorV1::Protection);
            }
        }
        if !leader_exited {
            match child_exited_without_reaping(&child) {
                Ok(exited) => leader_exited = exited,
                Err(_) => {
                    return fail_after_spawn(&mut child, GitCredentialErrorV1::Protection);
                }
            }
        }
        if leader_exited && stdout_closed && stderr_closed {
            break;
        }
        if Instant::now() >= deadline {
            return fail_after_spawn(&mut child, GitCredentialErrorV1::Protection);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Keep the leader's PID reserved until SIGKILL has targeted its own process
    // group. try_wait would reap the leader and allow PID/group-ID reuse before
    // a later timeout or descendant cleanup could signal the group.
    let status = terminate_child_group(&mut child).map_err(GitChildFailureV1::after)?;
    if let Some(home) = &network_home {
        home.verify().map_err(GitChildFailureV1::after)?;
    }
    if status.code().is_none() {
        return Err(GitChildFailureV1::after(GitCredentialErrorV1::Protection));
    }
    Ok(GitChildOutputV1 {
        stdout: stdout_body,
        status,
    })
}

#[cfg(unix)]
fn child_exited_without_reaping(child: &std::process::Child) -> Result<bool, GitCredentialErrorV1> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(GitCredentialErrorV1::Protection);
    }
    Ok(unsafe { info.si_pid() } == child.id() as libc::pid_t)
}

#[cfg(unix)]
fn fail_after_spawn<T>(
    child: &mut std::process::Child,
    error: GitCredentialErrorV1,
) -> Result<T, GitChildFailureV1> {
    // A cleanup failure remains post-spawn uncertainty; never turn it into a
    // successful result or expose child diagnostics.
    match terminate_child_group(child) {
        Ok(_) => Err(GitChildFailureV1::after(error)),
        Err(cleanup) => Err(GitChildFailureV1::after(cleanup)),
    }
}

#[cfg(unix)]
fn terminate_child_group(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus, GitCredentialErrorV1> {
    let group = -(child.id() as libc::pid_t);
    // This is the only signal: the leader has not been reaped by this runner,
    // so an unrelated process cannot acquire its PID/process-group ID yet.
    if unsafe { libc::kill(group, libc::SIGKILL) } != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error();
        // Darwin excludes zombies in killpg1's group iteration and reports
        // EPERM for a group containing only the unreaped exited leader.
        // This is not permission to ignore EPERM: require the owned leader's
        // WNOWAIT exit proof and, below, group disappearance after its reap.
        #[cfg(target_os = "macos")]
        let exited_only_candidate =
            errno == Some(libc::EPERM) && child_exited_without_reaping(child).unwrap_or(false);
        #[cfg(not(target_os = "macos"))]
        let exited_only_candidate = false;
        if errno != Some(libc::ESRCH) && !exited_only_candidate {
            // Reap the owned direct child even if group signalling failed.
            let killed = child.kill();
            let waited = child.wait();
            killed.map_err(|_| GitCredentialErrorV1::Protection)?;
            waited.map_err(|_| GitCredentialErrorV1::Protection)?;
            return Err(GitCredentialErrorV1::Protection);
        }
    }
    let status = child.wait().map_err(|_| GitCredentialErrorV1::Protection)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if unsafe { libc::kill(group, 0) } != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error();
            if errno == Some(libc::ESRCH) {
                return Ok(status);
            }
            // Darwin may still have orphan zombies waiting for init's reap;
            // wait for actual ESRCH, never treat EPERM itself as success.
            #[cfg(target_os = "macos")]
            let awaiting_reap = errno == Some(libc::EPERM);
            #[cfg(not(target_os = "macos"))]
            let awaiting_reap = false;
            if !awaiting_reap {
                return Err(GitCredentialErrorV1::Protection);
            }
        }
        // After reaping, only inspect; never signal an ID that could be reused.
        // A group still present at the deadline makes the result uncertain.
        if Instant::now() >= deadline {
            return Err(GitCredentialErrorV1::Protection);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn anonymous_pipe() -> Result<(File, File), GitCredentialErrorV1> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    for fd in fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

#[cfg(unix)]
fn duplicate_cloexec_fd(fd: RawFd) -> Result<File, GitCredentialErrorV1> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

#[cfg(unix)]
fn write_pipe_until(
    pipe: &mut File,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), GitCredentialErrorV1> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(GitCredentialErrorV1::Protection);
    }
    while !bytes.is_empty() {
        let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if count > 0 {
            bytes = &bytes[count as usize..];
        } else if count < 0
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
        {
            if Instant::now() >= deadline {
                return Err(GitCredentialErrorV1::Protection);
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut poll, 1, 10);
            }
        } else {
            return Err(GitCredentialErrorV1::Protection);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> Result<(), GitCredentialErrorV1> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(GitCredentialErrorV1::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn drain_child_output(
    pipe: &mut impl Read,
    output: &mut Zeroizing<Vec<u8>>,
) -> Result<bool, GitCredentialErrorV1> {
    let mut block = Zeroizing::new([0_u8; 8192]);
    loop {
        match pipe.read(block.as_mut()) {
            Ok(0) => {
                std::str::from_utf8(output.as_slice())
                    .map_err(|_| GitCredentialErrorV1::Protection)?;
                return Ok(true);
            }
            Ok(count) => {
                if output.len().saturating_add(count) as u64 > MAX_CHILD_OUTPUT_BYTES {
                    return Err(GitCredentialErrorV1::Protection);
                }
                output.extend_from_slice(&block[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(_) => return Err(GitCredentialErrorV1::Protection),
        }
    }
}

#[cfg(unix)]
fn checked_directory(fd: RawFd, strict_private: bool) -> Result<(), GitCredentialErrorV1> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0
        || stat.st_uid != unsafe { libc::geteuid() }
        || stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_mode & (if strict_private { 0o077 } else { 0o022 }) != 0
    {
        return Err(GitCredentialErrorV1::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path, private: bool) -> Result<File, GitCredentialErrorV1> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    checked_directory(file.as_raw_fd(), private)?;
    Ok(file)
}

#[cfg(unix)]
fn open_directory_at(
    parent: RawFd,
    name: &[u8],
    create: bool,
) -> Result<File, GitCredentialErrorV1> {
    let name = std::ffi::CString::new(name).map_err(|_| GitCredentialErrorV1::Protection)?;
    if create && unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    checked_directory(file.as_raw_fd(), true)?;
    Ok(file)
}

#[cfg(unix)]
struct KeyDirectoryGuard {
    home_path: PathBuf,
    home: File,
    polaris: File,
    keydir: File,
}

#[cfg(unix)]
impl KeyDirectoryGuard {
    fn as_raw_fd(&self) -> RawFd {
        self.keydir.as_raw_fd()
    }
    fn sync_all(&self) -> std::io::Result<()> {
        self.keydir.sync_all()
    }
}

#[cfg(unix)]
fn verify_named_key(parent: &KeyDirectoryGuard, file: &File) -> Result<(), GitCredentialErrorV1> {
    // The pinned descriptor chain is re-opened by the same names below; each
    // object must retain the identity held while the key was created/read.
    let named_home = open_directory(&parent.home_path, false)?;
    let named_polaris = open_directory_at(named_home.as_raw_fd(), b".polaris", false)?;
    let named_keydir = open_directory_at(
        parent.polaris.as_raw_fd(),
        b"git-credential-bindings-v1",
        false,
    )?;
    for (held, named) in [
        (&parent.home, &named_home),
        (&parent.polaris, &named_polaris),
        (&parent.keydir, &named_keydir),
    ] {
        let left = held.metadata()?;
        let right = named.metadata()?;
        if (
            left.dev(),
            left.ino(),
            left.uid(),
            left.mode(),
            left.nlink(),
        ) != (
            right.dev(),
            right.ino(),
            right.uid(),
            right.mode(),
            right.nlink(),
        ) {
            return Err(GitCredentialErrorV1::Protection);
        }
    }
    let name = std::ffi::CString::new("fingerprint.key").unwrap();
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(GitCredentialErrorV1::Protection);
    }
    let reopened = unsafe { File::from_raw_fd(fd) };
    let before = file.metadata()?;
    let after = reopened.metadata()?;
    if (
        before.dev(),
        before.ino(),
        before.uid(),
        before.mode(),
        before.nlink(),
        before.len(),
    ) != (
        after.dev(),
        after.ino(),
        after.uid(),
        after.mode(),
        after.nlink(),
        after.len(),
    ) {
        return Err(GitCredentialErrorV1::Protection);
    }
    Ok(())
}

#[cfg(unix)]
fn credential_key_directory(
    path: &Path,
    create: bool,
) -> Result<KeyDirectoryGuard, GitCredentialErrorV1> {
    if path.file_name().map(|name| name.as_bytes()) != Some(b"fingerprint.key") {
        return Err(GitCredentialErrorV1::Protection);
    }
    let directory = path.parent().ok_or(GitCredentialErrorV1::Protection)?;
    let polaris = directory.parent().ok_or(GitCredentialErrorV1::Protection)?;
    let home = polaris.parent().ok_or(GitCredentialErrorV1::Protection)?;
    if !home.is_absolute() || !matches!(home.canonicalize(), Ok(ref value) if value == home) {
        return Err(GitCredentialErrorV1::Protection);
    }
    let home_path = home.to_path_buf();
    let home = open_directory(home, false)?;
    let polaris = open_directory_at(home.as_raw_fd(), b".polaris", false)?;
    let keydir = open_directory_at(polaris.as_raw_fd(), b"git-credential-bindings-v1", create)?;
    if directory.file_name().map(|name| name.as_bytes()) != Some(b"git-credential-bindings-v1") {
        return Err(GitCredentialErrorV1::Protection);
    }
    Ok(KeyDirectoryGuard {
        home_path,
        home,
        polaris,
        keydir,
    })
}

#[cfg(unix)]
fn load_or_create_binding_key(
    path: &Path,
) -> Result<Zeroizing<[u8; FINGERPRINT_KEY_BYTES]>, GitCredentialErrorV1> {
    let parent = credential_key_directory(path, true)?;
    let name = std::ffi::CString::new("fingerprint.key").unwrap();
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )
    };
    let mut file = match fd {
        fd if fd >= 0 => {
            let mut file = unsafe { File::from_raw_fd(fd) };
            let mut key = Zeroizing::new([0_u8; FINGERPRINT_KEY_BYTES]);
            rand::SystemRandom::new()
                .fill(key.as_mut())
                .map_err(|_| GitCredentialErrorV1::Protection)?;
            file.write_all(&key[..])?;
            file.sync_all()?;
            parent.sync_all()?;
            parent.polaris.sync_all()?;
            file.seek(SeekFrom::Start(0))?;
            file
        }
        _ => {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            unsafe { File::from_raw_fd(fd) }
        }
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != FINGERPRINT_KEY_BYTES as u64
    {
        return Err(GitCredentialErrorV1::Protection);
    }
    let mut key = Zeroizing::new([0_u8; FINGERPRINT_KEY_BYTES]);
    file.read_exact(key.as_mut())?;
    if file.read(&mut [0_u8; 1])? != 0 {
        return Err(GitCredentialErrorV1::Protection);
    }
    verify_named_key(&parent, &file)?;
    Ok(key)
}

#[cfg(all(unix, any(target_os = "macos", test)))]
fn load_existing_binding_key(
    path: &Path,
) -> Result<Zeroizing<[u8; FINGERPRINT_KEY_BYTES]>, GitCredentialErrorV1> {
    let parent =
        credential_key_directory(path, false).map_err(|_| GitCredentialErrorV1::BindingChanged)?;
    let name = std::ffi::CString::new("fingerprint.key").unwrap();
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(GitCredentialErrorV1::BindingChanged);
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != FINGERPRINT_KEY_BYTES as u64
    {
        return Err(GitCredentialErrorV1::BindingChanged);
    }
    let mut file = file;
    let mut key = Zeroizing::new([0_u8; FINGERPRINT_KEY_BYTES]);
    file.read_exact(key.as_mut())?;
    if file.read(&mut [0_u8; 1])? != 0 {
        return Err(GitCredentialErrorV1::BindingChanged);
    }
    verify_named_key(&parent, &file).map_err(|_| GitCredentialErrorV1::BindingChanged)?;
    Ok(key)
}

// Refuse before inspecting paths, creating a key, or preparing a child.
#[cfg(not(unix))]
fn load_or_create_binding_key(
    _path: &Path,
) -> Result<Zeroizing<[u8; FINGERPRINT_KEY_BYTES]>, GitCredentialErrorV1> {
    Err(GitCredentialErrorV1::UnavailablePlatform)
}

#[cfg(not(unix))]
fn run_git_child(
    _git: &ExecutableIdentityV1,
    _askpass: Option<&ExecutableIdentityV1>,
    _material: Option<&GitCredentialMaterialV1>,
    _args: &[std::ffi::OsString],
    _index: Option<&Path>,
    _timeout: Duration,
) -> Result<GitChildOutputV1, GitChildFailureV1> {
    Err(GitChildFailureV1::before(
        GitCredentialErrorV1::UnavailablePlatform,
    ))
}

fn valid_hex32(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::data::CFData;
    use core_foundation::date::CFDate;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::{CFNumber, CFNumberIsFloatType};
    use core_foundation::string::{CFString, CFStringRef};
    use security_framework::access_control::SecAccessControl;
    use security_framework_sys::base::errSecItemNotFound;
    use security_framework_sys::item::*;
    use security_framework_sys::keychain_item::SecItemCopyMatching;

    unsafe extern "C" {
        static kSecAttrModificationDate: CFStringRef;
        static kSecValuePersistentRef: CFStringRef;
        static kSecAttrProtocolHTTPS: CFStringRef;
        static kSecAttrAuthenticationTypeDefault: CFStringRef;
        static kSecAttrAuthenticationTypeHTTPBasic: CFStringRef;
    }

    struct Candidate {
        reference: Vec<u8>,
        attribute_bytes: usize,
        modified_bits: u64,
        auth: AuthenticationTypeV1,
        security_domain: OptionalAttributeHashV1,
        access_group: OptionalAttributeHashV1,
    }

    fn key(value: CFStringRef) -> CFString {
        unsafe { CFString::wrap_under_get_rule(value) }
    }
    fn type_value(value: CFStringRef) -> CFType {
        key(value).into_CFType()
    }
    fn query_base(remote: &CanonicalHttpsRemoteV1, account: &str) -> Vec<(CFString, CFType)> {
        vec![
            (
                key(unsafe { kSecClass }),
                type_value(unsafe { kSecClassInternetPassword }),
            ),
            (
                key(unsafe { kSecUseDataProtectionKeychain }),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                key(unsafe { kSecAttrSynchronizable }),
                CFBoolean::false_value().into_CFType(),
            ),
            (
                key(unsafe { kSecAttrServer }),
                CFString::from(remote.host()).into_CFType(),
            ),
            (
                key(unsafe { kSecAttrAccount }),
                CFString::from(account).into_CFType(),
            ),
            (
                key(unsafe { kSecAttrPath }),
                CFString::from(remote.path()).into_CFType(),
            ),
            (
                key(unsafe { kSecAttrPort }),
                CFNumber::from(i32::from(remote.port())).into_CFType(),
            ),
            (
                key(unsafe { kSecAttrProtocol }),
                type_value(unsafe { kSecAttrProtocolHTTPS }),
            ),
        ]
    }
    fn query(pairs: &[(CFString, CFType)]) -> CFDictionary<CFString, CFType> {
        CFDictionary::from_CFType_pairs(pairs)
    }
    fn value<'a>(
        row: &'a CFDictionary<CFString, CFType>,
        key_ref: CFStringRef,
    ) -> Result<core_foundation::base::ItemRef<'a, CFType>, GitCredentialErrorV1> {
        row.find(key(key_ref)).ok_or(GitCredentialErrorV1::Keychain)
    }
    fn string(
        row: &CFDictionary<CFString, CFType>,
        key_ref: CFStringRef,
    ) -> Result<String, GitCredentialErrorV1> {
        value(row, key_ref)?
            .downcast::<CFString>()
            .map(|item| item.to_string())
            .ok_or(GitCredentialErrorV1::Keychain)
    }
    fn data(
        row: &CFDictionary<CFString, CFType>,
        key_ref: CFStringRef,
    ) -> Result<Vec<u8>, GitCredentialErrorV1> {
        value(row, key_ref)?
            .downcast::<CFData>()
            .map(|item| item.bytes().to_vec())
            .ok_or(GitCredentialErrorV1::Keychain)
    }
    fn number(
        row: &CFDictionary<CFString, CFType>,
        key_ref: CFStringRef,
    ) -> Result<i32, GitCredentialErrorV1> {
        value(row, key_ref)?
            .downcast::<CFNumber>()
            .and_then(|item| item.to_i32())
            .ok_or(GitCredentialErrorV1::Keychain)
    }
    fn bool_value(
        row: &CFDictionary<CFString, CFType>,
        key_ref: CFStringRef,
    ) -> Result<bool, GitCredentialErrorV1> {
        let value = value(row, key_ref)?;
        if let Some(value) = value.downcast::<CFBoolean>() {
            return Ok(bool::from(value));
        }
        if key_ref == unsafe { kSecAttrSynchronizable }
            && let Some(number) = value.downcast::<CFNumber>()
            && unsafe { CFNumberIsFloatType(number.as_concrete_TypeRef()) } == 0
        {
            return match number.to_i64() {
                Some(0) => Ok(false),
                Some(1) => Ok(true),
                _ => Err(GitCredentialErrorV1::Keychain),
            };
        }
        Err(GitCredentialErrorV1::Keychain)
    }
    fn optional_hash(
        row: &CFDictionary<CFString, CFType>,
        key_ref: CFStringRef,
        allow_empty: bool,
    ) -> Result<OptionalAttributeHashV1, GitCredentialErrorV1> {
        let Some(value) = row.find(key(key_ref)) else {
            return Ok(OptionalAttributeHashV1::Absent);
        };
        let value = value
            .downcast::<CFString>()
            .ok_or(GitCredentialErrorV1::Keychain)?
            .to_string();
        if !allow_empty && value.is_empty() {
            return Err(GitCredentialErrorV1::Keychain);
        }
        if key_ref == unsafe { kSecAttrSecurityDomain } && !value.is_empty() {
            return Err(GitCredentialErrorV1::Keychain);
        }
        Ok(OptionalAttributeHashV1::Present {
            sha256: hex(digest::digest(&digest::SHA256, value.as_bytes()).as_ref()),
        })
    }
    fn all_attribute_bytes(
        row: &CFDictionary<CFString, CFType>,
    ) -> Result<usize, GitCredentialErrorV1> {
        let (keys, values) = row.get_keys_and_values();
        let mut total = 0usize;
        for (name, value) in keys.into_iter().zip(values) {
            let name = unsafe { CFType::wrap_under_get_rule(name as CFTypeRef) }
                .downcast::<CFString>()
                .ok_or(GitCredentialErrorV1::Keychain)?;
            let value = unsafe { CFType::wrap_under_get_rule(value as CFTypeRef) };
            total = total
                .checked_add(name.to_string().len())
                .ok_or(GitCredentialErrorV1::Keychain)?;
            let bytes = if name == key(unsafe { kSecAttrAccessControl }) {
                if value.type_of() != SecAccessControl::type_id() {
                    return Err(GitCredentialErrorV1::Keychain);
                }
                // Public Security APIs expose no serialized-size projection
                // for this OS-owned opaque object. Never describe or persist it.
                0
            } else if let Some(value) = value.downcast::<CFString>() {
                value.to_string().len()
            } else if let Some(value) = value.downcast::<CFData>() {
                value.bytes().len()
            } else if value.downcast::<CFNumber>().is_some()
                || value.downcast::<CFBoolean>().is_some()
                || value.downcast::<CFDate>().is_some()
            {
                8
            } else {
                return Err(GitCredentialErrorV1::Keychain);
            };
            total = total
                .checked_add(bytes)
                .ok_or(GitCredentialErrorV1::Keychain)?;
            if total > MAX_ATTRIBUTE_BYTES {
                return Err(GitCredentialErrorV1::Keychain);
            }
        }
        Ok(total)
    }
    fn parse_row(
        row: &CFDictionary<CFString, CFType>,
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
    ) -> Result<Candidate, GitCredentialErrorV1> {
        let server = string(row, unsafe { kSecAttrServer })?;
        let item_account = string(row, unsafe { kSecAttrAccount })?;
        let path = string(row, unsafe { kSecAttrPath })?;
        if server != remote.host()
            || item_account != account
            || path != remote.path()
            || number(row, unsafe { kSecAttrPort })? != i32::from(remote.port())
            || string(row, unsafe { kSecAttrProtocol })? != key(unsafe { kSecAttrProtocolHTTPS })
            || bool_value(row, unsafe { kSecAttrSynchronizable })?
        {
            return Err(GitCredentialErrorV1::Keychain);
        }
        let auth = match string(row, unsafe { kSecAttrAuthenticationType })?.as_str() {
            value if value == key(unsafe { kSecAttrAuthenticationTypeDefault }) => {
                AuthenticationTypeV1::Default
            }
            value if value == key(unsafe { kSecAttrAuthenticationTypeHTTPBasic }) => {
                AuthenticationTypeV1::HttpBasic
            }
            _ => return Err(GitCredentialErrorV1::Keychain),
        };
        let date = value(row, unsafe { kSecAttrModificationDate })?
            .downcast::<CFDate>()
            .ok_or(GitCredentialErrorV1::Keychain)?;
        let reference = data(row, unsafe { kSecValuePersistentRef })?;
        let security_domain = optional_hash(row, unsafe { kSecAttrSecurityDomain }, true)?;
        let access_group = optional_hash(row, unsafe { kSecAttrAccessGroup }, false)?;
        let attribute_bytes = all_attribute_bytes(row)?;
        Ok(Candidate {
            reference,
            attribute_bytes,
            modified_bits: date.abs_time().to_bits(),
            auth,
            security_domain,
            access_group,
        })
    }
    fn candidates(
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
    ) -> Result<Vec<Candidate>, GitCredentialErrorV1> {
        let mut pairs = query_base(remote, account);
        pairs.extend([
            (
                key(unsafe { kSecReturnAttributes }),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                key(unsafe { kSecReturnPersistentRef }),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                key(unsafe { kSecMatchLimit }),
                CFNumber::from(65).into_CFType(),
            ),
        ]);
        let dictionary = query(&pairs);
        let mut result = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(dictionary.as_concrete_TypeRef(), &mut result) };
        if status == errSecItemNotFound {
            return Ok(Vec::new());
        }
        if status != 0 || result.is_null() {
            return Err(GitCredentialErrorV1::Keychain);
        }
        let result = unsafe { CFType::wrap_under_create_rule(result) };
        let rows = result
            .downcast::<CFArray>()
            .ok_or(GitCredentialErrorV1::Keychain)?;
        let count = rows.len();
        if count < 0 || count as usize > MAX_CANDIDATES {
            return Err(GitCredentialErrorV1::Keychain);
        }
        let mut total = 0usize;
        let mut parsed = Vec::with_capacity(count as usize);
        for row in rows.iter() {
            let row = unsafe { CFType::wrap_under_get_rule(*row as CFTypeRef) };
            let raw_row = row
                .downcast::<CFDictionary>()
                .ok_or(GitCredentialErrorV1::Keychain)?;
            let row = unsafe {
                CFDictionary::<CFString, CFType>::wrap_under_get_rule(raw_row.as_concrete_TypeRef())
            };
            let candidate = parse_row(&row, remote, account)?;
            total = total
                .checked_add(candidate.attribute_bytes)
                .ok_or(GitCredentialErrorV1::Keychain)?;
            if total > MAX_ATTRIBUTE_BYTES {
                return Err(GitCredentialErrorV1::Keychain);
            }
            if parsed
                .iter()
                .any(|existing: &Candidate| existing.reference == candidate.reference)
            {
                return Err(GitCredentialErrorV1::Keychain);
            }
            parsed.push(candidate);
        }
        Ok(parsed)
    }
    fn snapshot(
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
        reference: &[u8],
    ) -> Result<(Candidate, Zeroizing<Vec<u8>>), GitCredentialErrorV1> {
        // A persistent reference identifies the item. Recheck all remote,
        // account and non-sync attributes from the same returned dictionary.
        let mut pairs = vec![
            (
                key(unsafe { kSecClass }),
                type_value(unsafe { kSecClassInternetPassword }),
            ),
            (
                key(unsafe { kSecUseDataProtectionKeychain }),
                CFBoolean::true_value().into_CFType(),
            ),
        ];
        pairs.extend([
            (
                key(unsafe { kSecValuePersistentRef }),
                CFData::from_buffer(reference).into_CFType(),
            ),
            (
                key(unsafe { kSecReturnAttributes }),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                key(unsafe { kSecReturnPersistentRef }),
                CFBoolean::true_value().into_CFType(),
            ),
            (
                key(unsafe { kSecReturnData }),
                CFBoolean::true_value().into_CFType(),
            ),
        ]);
        let dictionary = query(&pairs);
        let mut result = std::ptr::null();
        if unsafe { SecItemCopyMatching(dictionary.as_concrete_TypeRef(), &mut result) } != 0
            || result.is_null()
        {
            return Err(GitCredentialErrorV1::BindingChanged);
        }
        let result = unsafe { CFType::wrap_under_create_rule(result) };
        let raw_row = result
            .downcast::<CFDictionary>()
            .ok_or(GitCredentialErrorV1::Keychain)?;
        let row = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(raw_row.as_concrete_TypeRef())
        };
        let candidate = parse_row(&row, remote, account)?;
        if candidate.reference != reference {
            return Err(GitCredentialErrorV1::BindingChanged);
        }
        Ok((
            candidate,
            Zeroizing::new(data(&row, unsafe { kSecValueData })?),
        ))
    }
    fn version(key: &[u8; FINGERPRINT_KEY_BYTES], candidate: &Candidate, secret: &[u8]) -> String {
        let signing = hmac::Key::new(hmac::HMAC_SHA256, key);
        let mut context = hmac::Context::with_key(&signing);
        context.update(&candidate.reference);
        context.update(&candidate.modified_bits.to_be_bytes());
        context.update(&(secret.len() as u64).to_be_bytes());
        context.update(secret);
        hex(context.sign().as_ref())
    }
    fn binding(
        owner: &GitCredentialOwnerV1,
        remote: CanonicalHttpsRemoteV1,
        account: &str,
        generation: u64,
        candidate: &Candidate,
        secret: &[u8],
        key: &[u8; FINGERPRINT_KEY_BYTES],
    ) -> GitCredentialBindingV1 {
        GitCredentialBindingV1 {
            namespace: NAMESPACE.into(),
            credential_item_id: hex(digest::digest(&digest::SHA256, &candidate.reference).as_ref()),
            credential_item_version: version(key, candidate, secret),
            remote,
            account: account.into(),
            authentication_type: candidate.auth,
            security_domain: candidate.security_domain.clone(),
            access_group: candidate.access_group.clone(),
            synchronizable: false,
            generation,
            git: owner.git.clone(),
            askpass: owner.askpass.clone(),
        }
    }
    fn choice(candidate: &Candidate) -> CredentialCandidateChoiceV1 {
        CredentialCandidateChoiceV1 {
            item_id_prefix: hex(digest::digest(&digest::SHA256, &candidate.reference).as_ref())
                [..16]
                .to_owned(),
        }
    }
    pub(super) fn candidate_choices(
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
    ) -> Result<Vec<CredentialCandidateChoiceV1>, GitCredentialErrorV1> {
        candidates(remote, account).map(|items| items.iter().map(choice).collect())
    }
    pub(super) fn select(
        owner: &GitCredentialOwnerV1,
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
        generation: u64,
        choice: Option<&str>,
    ) -> Result<(GitCredentialBindingV1, GitCredentialMaterialV1), GitCredentialErrorV1> {
        if generation == 0 {
            return Err(GitCredentialErrorV1::InvalidBinding);
        }
        let candidates = candidates(remote, account)?;
        let candidate = match candidates.len() {
            0 => return Err(GitCredentialErrorV1::Missing),
            1 => candidates.into_iter().next().unwrap(),
            _ => {
                let selected = choice.ok_or(GitCredentialErrorV1::Ambiguous)?;
                if selected.len() != 16 || !selected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(GitCredentialErrorV1::Ambiguous);
                }
                let mut found = candidates.into_iter().filter(|candidate| {
                    hex(digest::digest(&digest::SHA256, &candidate.reference).as_ref())
                        .starts_with(selected)
                });
                let candidate = found.next().ok_or(GitCredentialErrorV1::Ambiguous)?;
                if found.next().is_some() {
                    return Err(GitCredentialErrorV1::Ambiguous);
                }
                candidate
            }
        };
        let (candidate, secret) = snapshot(remote, account, &candidate.reference)?;
        let key = owner.binding_key()?;
        let binding = binding(
            owner,
            remote.clone(),
            account,
            generation,
            &candidate,
            &secret,
            &key,
        );
        binding.validate()?;
        Ok((
            binding,
            GitCredentialMaterialV1 {
                username: Zeroizing::new(account.as_bytes().to_vec()),
                password: secret,
            },
        ))
    }
    pub(super) fn revalidate(
        owner: &GitCredentialOwnerV1,
        binding_value: &GitCredentialBindingV1,
    ) -> Result<(), GitCredentialErrorV1> {
        read_bound_material(owner, binding_value).map(drop)
    }

    pub(super) fn read_bound_material(
        owner: &GitCredentialOwnerV1,
        binding_value: &GitCredentialBindingV1,
    ) -> Result<GitCredentialMaterialV1, GitCredentialErrorV1> {
        binding_value.validate()?;
        owner.revalidate_binaries()?;
        let key = load_existing_binding_key(&owner.binding_key_path)?;
        let candidates = candidates(&binding_value.remote, &binding_value.account)?;
        let matching: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| {
                hex(digest::digest(&digest::SHA256, &candidate.reference).as_ref())
                    == binding_value.credential_item_id
            })
            .collect();
        if matching.len() != 1 {
            return Err(GitCredentialErrorV1::BindingChanged);
        }
        let (candidate, secret) = snapshot(
            &binding_value.remote,
            &binding_value.account,
            &matching[0].reference,
        )?;
        let current = binding(
            owner,
            binding_value.remote.clone(),
            &binding_value.account,
            binding_value.generation,
            &candidate,
            &secret,
            &key,
        );
        if current == *binding_value {
            Ok(GitCredentialMaterialV1 {
                username: Zeroizing::new(binding_value.account.as_bytes().to_vec()),
                password: secret,
            })
        } else {
            Err(GitCredentialErrorV1::BindingChanged)
        }
    }

    #[cfg(test)]
    mod attribute_tests {
        use super::*;

        #[test]
        fn dp_sync_integer_representation_is_exact_and_never_accepts_true() {
            let remote = CanonicalHttpsRemoteV1::parse("https://owned.invalid:443/owned").unwrap();
            for (value, valid) in [
                (CFNumber::from(0_i64).into_CFType(), true),
                (CFNumber::from(1_i64).into_CFType(), false),
                (CFNumber::from(-1_i64).into_CFType(), false),
                (CFNumber::from(2_i64).into_CFType(), false),
                (CFNumber::from(i64::MAX).into_CFType(), false),
                (CFNumber::from(0.0_f64).into_CFType(), false),
                (CFNumber::from(0.5_f64).into_CFType(), false),
                (CFString::new("0").into_CFType(), false),
            ] {
                let mut pairs = row();
                pairs.retain(|(name, _)| *name != key(unsafe { kSecAttrSynchronizable }));
                pairs.push((key(unsafe { kSecAttrSynchronizable }), value));
                assert_eq!(
                    parse_row(&query(&pairs), &remote, "owned-account").is_ok(),
                    valid
                );
            }
        }
        #[test]
        fn dp_attribute_projection_enforces_exact_budget_and_only_os_acl_is_opaque() {
            for (length, valid) in [
                (MAX_ATTRIBUTE_BYTES - 1, true),
                (MAX_ATTRIBUTE_BYTES, false),
            ] {
                let pairs = [(
                    CFString::new("x"),
                    CFData::from_buffer(&vec![b'x'; length]).into_CFType(),
                )];
                assert_eq!(all_attribute_bytes(&query(&pairs)).is_ok(), valid);
            }
            let acl = SecAccessControl::create_with_flags(0).unwrap();
            let valid = [(
                key(unsafe { kSecAttrAccessControl }),
                acl.clone().into_CFType(),
            )];
            assert!(all_attribute_bytes(&query(&valid)).is_ok());
            let wrong_name = [(CFString::new("unknown-opaque"), acl.into_CFType())];
            assert!(all_attribute_bytes(&query(&wrong_name)).is_err());
            let wrong_type = [(
                key(unsafe { kSecAttrAccessControl }),
                CFString::new("not-acl").into_CFType(),
            )];
            assert!(all_attribute_bytes(&query(&wrong_type)).is_err());
        }

        fn row() -> Vec<(CFString, CFType)> {
            vec![
                (
                    key(unsafe { kSecAttrServer }),
                    CFString::new("owned.invalid").into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrAccount }),
                    CFString::new("owned-account").into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrPath }),
                    CFString::new("/owned").into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrPort }),
                    CFNumber::from(443_i32).into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrProtocol }),
                    type_value(unsafe { kSecAttrProtocolHTTPS }),
                ),
                (
                    key(unsafe { kSecAttrSynchronizable }),
                    CFBoolean::false_value().into_CFType(),
                ),
                (
                    key(unsafe { kSecAttrAuthenticationType }),
                    type_value(unsafe { kSecAttrAuthenticationTypeDefault }),
                ),
                (
                    key(unsafe { kSecAttrModificationDate }),
                    CFDate::new(0.0).into_CFType(),
                ),
                (
                    key(unsafe { kSecValuePersistentRef }),
                    CFData::from_buffer(b"synthetic-reference").into_CFType(),
                ),
            ]
        }
        #[test]
        fn dp_attribute_parser_rejects_required_drift_missing_and_wrong_type() {
            let remote = CanonicalHttpsRemoteV1::parse("https://owned.invalid:443/owned").unwrap();
            assert!(parse_row(&query(&row()), &remote, "owned-account").is_ok());
            for (field, changed) in [
                (
                    unsafe { kSecAttrServer },
                    CFString::new("different.invalid").into_CFType(),
                ),
                (
                    unsafe { kSecAttrAccount },
                    CFString::new("different-account").into_CFType(),
                ),
                (
                    unsafe { kSecAttrPath },
                    CFString::new("/different").into_CFType(),
                ),
                (
                    unsafe { kSecAttrPort },
                    CFNumber::from(444_i32).into_CFType(),
                ),
                (
                    unsafe { kSecAttrProtocol },
                    CFString::new("http").into_CFType(),
                ),
                (
                    unsafe { kSecAttrSynchronizable },
                    CFBoolean::true_value().into_CFType(),
                ),
                (
                    unsafe { kSecAttrAuthenticationType },
                    CFString::new("unsupported").into_CFType(),
                ),
            ] {
                let field = key(field);
                for replacement in [
                    Some(changed),
                    None,
                    Some(CFData::from_buffer(b"wrong-type").into_CFType()),
                ] {
                    let mut pairs = row();
                    pairs.retain(|(name, _)| *name != field);
                    if let Some(value) = replacement {
                        pairs.push((field.clone(), value));
                    }
                    assert!(parse_row(&query(&pairs), &remote, "owned-account").is_err());
                }
            }
        }
        #[test]
        fn dp_attribute_parser_preserves_absence_empty_and_access_group_hash() {
            let remote = CanonicalHttpsRemoteV1::parse("https://owned.invalid:443/owned").unwrap();
            let absent = parse_row(&query(&row()), &remote, "owned-account").unwrap();
            assert!(matches!(
                absent.security_domain,
                OptionalAttributeHashV1::Absent
            ));
            assert!(matches!(
                absent.access_group,
                OptionalAttributeHashV1::Absent
            ));
            let mut pairs = row();
            pairs.push((
                key(unsafe { kSecAttrSecurityDomain }),
                CFString::new("").into_CFType(),
            ));
            pairs.push((
                key(unsafe { kSecAttrAccessGroup }),
                CFString::new("owned-group").into_CFType(),
            ));
            let present = parse_row(&query(&pairs), &remote, "owned-account").unwrap();
            assert!(present.security_domain != absent.security_domain);
            assert!(present.access_group != absent.access_group);
            assert!(
                matches!(present.security_domain, OptionalAttributeHashV1::Present { sha256 }
                if sha256 == hex(digest::digest(&digest::SHA256, b"").as_ref()))
            );
            assert!(
                matches!(present.access_group, OptionalAttributeHashV1::Present { sha256 }
                if sha256 == hex(digest::digest(&digest::SHA256, b"owned-group").as_ref()))
            );
            for (field, changed) in [
                (
                    unsafe { kSecAttrSecurityDomain },
                    CFString::new("nonempty").into_CFType(),
                ),
                (
                    unsafe { kSecAttrSecurityDomain },
                    CFData::from_buffer(b"wrong-type").into_CFType(),
                ),
                (
                    unsafe { kSecAttrAccessGroup },
                    CFString::new("").into_CFType(),
                ),
                (
                    unsafe { kSecAttrAccessGroup },
                    CFBoolean::false_value().into_CFType(),
                ),
            ] {
                let mut pairs = row();
                pairs.push((key(field), changed));
                assert!(parse_row(&query(&pairs), &remote, "owned-account").is_err());
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl GitCredentialOwnerV1 {
    /// Reads the exact existing binding and fingerprint key only; never
    /// selects another item, creates state, or repairs a stale binding.
    pub fn read_bound_material(
        &self,
        binding: &GitCredentialBindingV1,
    ) -> Result<GitCredentialMaterialV1, GitCredentialErrorV1> {
        macos::read_bound_material(self, binding)
    }

    pub fn candidate_choices(
        &self,
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
    ) -> Result<Vec<CredentialCandidateChoiceV1>, GitCredentialErrorV1> {
        macos::candidate_choices(remote, account)
    }
    pub fn select(
        &self,
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
        generation: u64,
        choice: Option<&str>,
    ) -> Result<(GitCredentialBindingV1, GitCredentialMaterialV1), GitCredentialErrorV1> {
        macos::select(self, remote, account, generation, choice)
    }
    pub fn revalidate_binding(
        &self,
        binding: &GitCredentialBindingV1,
    ) -> Result<(), GitCredentialErrorV1> {
        macos::revalidate(self, binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_blob_identity_uses_git_header_and_raw_bytes() {
        assert_eq!(
            git_blob_oid_from_bytes(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            git_blob_oid_from_bytes(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_ne!(
            git_blob_oid_from_bytes(b"hello\n"),
            git_blob_oid_from_bytes(b"hello\r\n")
        );
    }

    #[test]
    fn remote_rejects_secret_and_implicit_port_forms() {
        for value in [
            "https://example.invalid/x",
            "https://u:p@example.invalid:443/x",
            "ssh://example.invalid:22/x",
            "https://example.invalid:443/x?q=1",
        ] {
            assert!(CanonicalHttpsRemoteV1::parse(value).is_err());
        }
        let remote = CanonicalHttpsRemoteV1::parse("https://example.invalid:443/a.git").unwrap();
        assert_eq!(remote.to_string(), "https://host-26789e203661:443");
        assert!(!format!("{remote:?}").contains("example.invalid"));
        assert!(!remote.to_string().contains("a.git"));
    }

    #[test]
    fn remote_wire_rejects_noncanonical_fields() {
        for value in [
            r#"{"host":"EXAMPLE.invalid","port":443,"path":"/a.git"}"#,
            r#"{"host":"example.invalid","port":443,"path":"/a/../b"}"#,
            r#"{"host":"example.invalid","port":443,"path":"/a.git","extra":true}"#,
        ] {
            assert!(serde_json::from_str::<CanonicalHttpsRemoteV1>(value).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn binding_key_is_exactly_owner_only_and_stable() {
        let root = tempfile::tempdir().unwrap();
        let polaris = root.path().join(".polaris");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&polaris)
            .unwrap();
        let path = root
            .path()
            .join(".polaris/git-credential-bindings-v1/fingerprint.key");
        let first = load_or_create_binding_key(&path).unwrap();
        let second = load_or_create_binding_key(&path).unwrap();
        assert_eq!(&*first, &*second);
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn binding_key_rejects_wrong_length() {
        let root = tempfile::tempdir().unwrap();
        let polaris = root.path().join(".polaris");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&polaris)
            .unwrap();
        let path = polaris.join("git-credential-bindings-v1/fingerprint.key");
        let _ = load_or_create_binding_key(&path).unwrap();
        std::fs::write(&path, [0_u8; 31]).unwrap();
        assert!(load_existing_binding_key(&path).is_err());
    }

    #[test]
    fn executable_structure_rejects_writable_or_relative_identity() {
        let identity = ExecutableIdentityV1 {
            path: PathBuf::from("relative"),
            device: 1,
            inode: 1,
            owner_uid: 0,
            mode: 0o755,
            sha256: "0".repeat(64),
        };
        assert!(identity.validate_structure().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn binding_key_rejects_unsafe_key_directory_permissions() {
        let root = tempfile::tempdir().unwrap();
        let polaris = root.path().join(".polaris");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&polaris)
            .unwrap();
        let path = polaris.join("git-credential-bindings-v1/fingerprint.key");
        let _ = load_or_create_binding_key(&path).unwrap();
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(load_existing_binding_key(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn binding_key_rejects_hardlink_and_symlink() {
        let root = tempfile::tempdir().unwrap();
        let polaris = root.path().join(".polaris");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&polaris)
            .unwrap();
        let path = polaris.join("git-credential-bindings-v1/fingerprint.key");
        let _ = load_or_create_binding_key(&path).unwrap();
        std::fs::hard_link(&path, root.path().join("copy")).unwrap();
        assert!(load_existing_binding_key(&path).is_err());
        std::fs::remove_file(root.path().join("copy")).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(root.path().join("elsewhere"), &path).unwrap();
        assert!(load_existing_binding_key(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn binding_key_rejects_replaced_parent() {
        let root = tempfile::tempdir().unwrap();
        let polaris = root.path().join(".polaris");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&polaris)
            .unwrap();
        let path = polaris.join("git-credential-bindings-v1/fingerprint.key");
        let _ = load_or_create_binding_key(&path).unwrap();
        let directory = path.parent().unwrap();
        std::fs::rename(directory, polaris.join("old-keydir")).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .unwrap();
        assert!(load_existing_binding_key(&path).is_err());
    }

    #[test]
    fn binding_rejects_generation_sync_and_attribute_hash_drift() {
        let executable = ExecutableIdentityV1 {
            path: PathBuf::from(if cfg!(windows) {
                r"C:\physical\tool"
            } else {
                "/physical/tool"
            }),
            device: 1,
            inode: 1,
            owner_uid: 0,
            mode: 0o755,
            sha256: "0".repeat(64),
        };
        let remote = CanonicalHttpsRemoteV1::parse("https://example.invalid:443/a.git").unwrap();
        let mut binding = GitCredentialBindingV1 {
            namespace: NAMESPACE.into(),
            credential_item_id: "1".repeat(64),
            credential_item_version: "2".repeat(64),
            remote,
            account: "account".into(),
            authentication_type: AuthenticationTypeV1::Default,
            security_domain: OptionalAttributeHashV1::Absent,
            access_group: OptionalAttributeHashV1::Absent,
            synchronizable: false,
            generation: 1,
            git: executable.clone(),
            askpass: executable,
        };
        assert!(binding.validate().is_ok());
        binding.generation = 0;
        assert!(binding.validate().is_err());
        binding.generation = 1;
        binding.synchronizable = true;
        assert!(binding.validate().is_err());
        binding.synchronizable = false;
        binding.security_domain = OptionalAttributeHashV1::Present {
            sha256: "bad".into(),
        };
        assert!(binding.validate().is_err());
    }
}

#[cfg(all(test, unix))]
mod runner_tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn identity(path: PathBuf) -> ExecutableIdentityV1 {
        ExecutableIdentityV1 {
            path,
            device: 1,
            inode: 2,
            owner_uid: 0,
            mode: 0o755,
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn windows_git_credentials_unavailable_before_executable_or_owner_access() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("tool.exe");
        std::fs::write(&binary, b"fixture, not an executable").unwrap();
        let missing = root.path().join("missing").join("tool.exe");
        for path in [&binary, &missing] {
            assert!(matches!(
                ExecutableIdentityV1::inspect(path),
                Err(GitCredentialErrorV1::UnavailablePlatform)
            ));
            assert!(matches!(
                identity(path.clone()).revalidate(),
                Err(GitCredentialErrorV1::UnavailablePlatform)
            ));
            assert!(matches!(
                GitCredentialOwnerV1::new(root.path(), path, path),
                Err(GitCredentialErrorV1::UnavailablePlatform)
            ));
        }
        assert_eq!(
            std::fs::read(&binary).unwrap(),
            b"fixture, not an executable"
        );
        assert!(!missing.parent().unwrap().exists());
        assert!(!root.path().join(".polaris").exists());
    }

    #[test]
    fn windows_git_credentials_unavailable_binding_key_preserves_paths() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("fingerprint.key");
        std::fs::write(&existing, b"existing fixture").unwrap();
        let missing = root.path().join("missing").join("fingerprint.key");
        for path in [&existing, &missing] {
            let owner = GitCredentialOwnerV1 {
                binding_key_path: path.clone(),
                git: identity(root.path().join("git.exe")),
                askpass: identity(root.path().join("askpass.exe")),
            };
            assert!(matches!(
                owner.binding_key(),
                Err(GitCredentialErrorV1::UnavailablePlatform)
            ));
        }
        assert_eq!(std::fs::read(&existing).unwrap(), b"existing fixture");
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn windows_git_credentials_unavailable_runners_report_before_spawn() {
        let root = tempfile::tempdir().unwrap();
        let index = root.path().join("index");
        std::fs::write(&index, b"original index fixture").unwrap();
        let owner = GitCredentialOwnerV1 {
            binding_key_path: root.path().join("missing").join("fingerprint.key"),
            git: identity(root.path().join("git.exe")),
            askpass: identity(root.path().join("askpass.exe")),
        };
        let material = GitCredentialMaterialV1 {
            username: Zeroizing::new(b"dummy-user".to_vec()),
            password: Zeroizing::new(b"dummy-password".to_vec()),
        };
        for result in [
            run_bound_local_git(&owner.git, &[], &index),
            owner.run_bound_git_output(&[], &material),
            owner.run_bound_git_output_at_index(&[], &material, &index),
        ] {
            assert!(matches!(
                result,
                Err(GitChildFailureV1 {
                    phase: GitChildFailurePhaseV1::BeforeSpawn,
                    error: GitCredentialErrorV1::UnavailablePlatform,
                })
            ));
        }
        assert!(matches!(
            owner.run_bound_git(&[], &material),
            Err(GitCredentialErrorV1::UnavailablePlatform)
        ));
        assert!(matches!(
            owner.run_bound_git_with_timeout(&[], &material, Duration::ZERO),
            Err(GitCredentialErrorV1::UnavailablePlatform)
        ));
        assert!(matches!(
            owner.revalidate_binaries(),
            Err(GitCredentialErrorV1::UnavailablePlatform)
        ));
        assert_eq!(std::fs::read(&index).unwrap(), b"original index fixture");
        assert!(!root.path().join("missing").exists());
    }
}
