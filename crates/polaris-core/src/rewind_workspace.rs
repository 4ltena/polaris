//! Durable, bounded workspace checkpoints used by file-and-context rewind.
//!
//! This owner is deliberately synchronous. Callers stop all Polaris writers
//! before capture/restore. A cross-process project lease is always acquired
//! before the authentication registry lock.

#![cfg(unix)]

use crate::isolated_workspace::{self, ExclusionReason, Limits, RegisteredSecrets, Snapshot};
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MANIFEST_VERSION: u32 = 1;
const TRANSACTION_VERSION: u32 = 1;
const NAMESPACE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct RewindLimits {
    pub max_entries: usize,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_retained_blob_bytes: u64,
    pub max_depth: usize,
    pub capture_deadline: Duration,
}

impl Default for RewindLimits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_files: 50_000,
            max_file_bytes: 32 * 1024 * 1024,
            max_manifest_bytes: 512 * 1024 * 1024,
            max_retained_blob_bytes: 512 * 1024 * 1024,
            max_depth: 128,
            capture_deadline: Duration::from_secs(2),
        }
    }
}

impl RewindLimits {
    fn snapshot(self) -> Limits {
        Limits {
            max_entries: self.max_entries,
            max_files: self.max_files,
            max_file_bytes: self.max_file_bytes,
            max_total_bytes: self.max_manifest_bytes,
            max_depth: self.max_depth,
        }
    }
}

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 16]);

        impl $name {
            fn random() -> Result<Self, WorkspaceError> {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
                Ok(Self(bytes))
            }

            fn encoded(self) -> String {
                self.0.iter().map(|byte| format!("{byte:02x}")).collect()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name))
                    .field(&self.encoded())
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.encoded())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.len() != 32
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(IdParseError);
                }
                let mut bytes = [0u8; 16];
                for (index, output) in bytes.iter_mut().enumerate() {
                    let offset = index * 2;
                    *output = u8::from_str_radix(&value[offset..offset + 2], 16)
                        .map_err(|_| IdParseError)?;
                }
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.encoded())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdParseError;

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid opaque rewind id")
    }
}

impl std::error::Error for IdParseError {}

opaque_id!(ManifestId);
opaque_id!(TransactionId);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreviewChangeKind {
    Created,
    Modified,
    Deleted,
    ModeChanged,
    TypeChanged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreviewChange {
    pub relative_path: PathBuf,
    pub kind: PreviewChangeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreviewBlockedReason {
    ProjectMismatch,
    IncompleteCapture,
    MissingBlob,
    CorruptBlob,
    PendingTransaction,
    UnsafeTopology,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub changes: Vec<PreviewChange>,
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
    pub restorable: bool,
    pub blocked_reason: Option<PreviewBlockedReason>,
    /// Stable owner-controlled parent; the concrete child is returned by restore.
    pub recovery_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreOutcome {
    pub transaction_id: TransactionId,
    pub restored_manifest_id: ManifestId,
    pub recovery_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackOutcome {
    pub workspace_manifest_id: ManifestId,
    pub recovery_path: PathBuf,
}

/// Durable transaction facts read while the project mutation lease is held.
/// The workspace binding is checked before this value is returned for either
/// terminal phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindTransactionReadback {
    pub transaction_id: TransactionId,
    pub candidate_session_id: Option<String>,
    pub expected_post: ManifestId,
    pub target_pre: ManifestId,
    pub restored_manifest_id: Option<ManifestId>,
    pub rollback_manifest_id: Option<ManifestId>,
    pub phase: RewindTransactionPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RewindTransactionPhase {
    Prepared,
    Applying,
    PendingActivation,
    Committed,
    RollingBack,
    RolledBack,
}

/// Evidence that a rewind branch can be admitted after its workspace change
/// crossed the durable commit point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindBranchTransactionEvidence {
    pub transaction_id: TransactionId,
    pub restored_manifest_id: ManifestId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveOutcome {
    Clean,
    RolledBack(Vec<RollbackOutcome>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Io,
    InvalidProject,
    InvalidStore,
    IneligiblePolicy,
    Busy,
    PendingTransaction,
    CaptureDeadline,
    CaptureLimit,
    IncompleteCapture,
    MissingManifest,
    CorruptManifest,
    MissingBlob,
    CorruptBlob,
    Conflict,
    CrossDevice,
    ProtectionUnavailable,
    RestoreFailed,
    InvalidTransaction,
    InvalidCandidateSession,
}

#[derive(Debug)]
pub struct WorkspaceError {
    pub kind: ErrorKind,
    pub path: Option<PathBuf>,
}

impl WorkspaceError {
    fn new(kind: ErrorKind) -> Self {
        Self { kind, path: None }
    }

    fn at(kind: ErrorKind, path: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            path: Some(path.into()),
        }
    }
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{:?}: {}", self.kind, path.display()),
            None => write!(f, "{:?}", self.kind),
        }
    }
}

impl std::error::Error for WorkspaceError {}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredEntry {
    path: PathBuf,
    kind: EntryKind,
    mode: u32,
    bytes: u64,
    hash: Option<[u8; 32]>,
    device: u64,
    inode: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl StoredEntry {
    fn same_value(&self, other: &Self) -> bool {
        self.path == other.path
            && self.kind == other.kind
            && self.mode == other.mode
            && self.bytes == other.bytes
            && self.hash == other.hash
    }

    fn same_observation(&self, other: &Self) -> bool {
        self.same_value(other)
            && self.device == other.device
            && self.inode == other.inode
            && self.mtime == other.mtime
            && self.ctime == other.ctime
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredExclusion {
    path: PathBuf,
    reason: StoredExclusionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum StoredExclusionReason {
    RepositoryMetadata,
    BuildCache,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredManifest {
    version: u32,
    id: ManifestId,
    project_root: PathBuf,
    root_device: u64,
    root_inode: u64,
    captured_unix_ms: u128,
    entries: Vec<StoredEntry>,
    exclusions: Vec<StoredExclusion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Namespace {
    version: u32,
    project_root: PathBuf,
    root_device: u64,
    root_inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum TransactionPhase {
    Prepared,
    Applying,
    PendingActivation,
    Committed,
    RollingBack,
    RolledBack,
}

impl From<TransactionPhase> for RewindTransactionPhase {
    fn from(value: TransactionPhase) -> Self {
        match value {
            TransactionPhase::Prepared => Self::Prepared,
            TransactionPhase::Applying => Self::Applying,
            TransactionPhase::PendingActivation => Self::PendingActivation,
            TransactionPhase::Committed => Self::Committed,
            TransactionPhase::RollingBack => Self::RollingBack,
            TransactionPhase::RolledBack => Self::RolledBack,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransactionMarker {
    version: u32,
    id: TransactionId,
    expected_post: ManifestId,
    #[serde(default)]
    observed_post_manifest: Option<ManifestId>,
    target_pre: ManifestId,
    candidate_session_id: Option<String>,
    restored_manifest: Option<ManifestId>,
    #[serde(default)]
    rollback_manifest: Option<ManifestId>,
    recovery_path: PathBuf,
    temporary_paths: Vec<PathBuf>,
    phase: TransactionPhase,
}

#[derive(Debug)]
pub struct WorkspaceOwner {
    project_root: PathBuf,
    store_root: PathBuf,
    limits: RewindLimits,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyBindingReservationV1 {
    version: u32,
    session_id: String,
}

/// Exists only inside the project lease and matching pending reservation.
/// Conversation storage requires this capability before replacing a sidecar
/// left by an interrupted first bind.
pub struct PendingLegacyBindingPermit<'a> {
    session_id: String,
    _lease: &'a ProjectLease,
}

impl PendingLegacyBindingPermit<'_> {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl WorkspaceOwner {
    pub fn for_project(project_root: &Path, limits: RewindLimits) -> Result<Self, WorkspaceError> {
        let store = crate::project::state_dir(project_root)
            .map_err(|_| WorkspaceError::new(ErrorKind::InvalidStore))?
            .join("rewind-workspace");
        Self::open(project_root, &store, limits)
    }

    pub fn open(
        project_root: &Path,
        private_store_root: &Path,
        limits: RewindLimits,
    ) -> Result<Self, WorkspaceError> {
        let project_root = fs::canonicalize(project_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidProject, project_root))?;
        let project_meta = fs::metadata(&project_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidProject, &project_root))?;
        if !project_meta.is_dir() || project_root.parent().is_none() {
            return Err(WorkspaceError::at(ErrorKind::InvalidProject, project_root));
        }
        if !private_store_root.is_absolute() {
            return Err(WorkspaceError::at(
                ErrorKind::InvalidStore,
                private_store_root,
            ));
        }
        private_dir_all(private_store_root)?;
        let store_root = fs::canonicalize(private_store_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, private_store_root))?;
        if store_root.starts_with(&project_root) || project_root.starts_with(&store_root) {
            return Err(WorkspaceError::at(ErrorKind::InvalidStore, store_root));
        }
        validate_private_dir(&store_root)?;
        for name in ["manifests", "blobs", "transactions", "recovery"] {
            private_dir_all(&store_root.join(name))?;
        }
        let owner = Self {
            project_root,
            store_root,
            limits,
        };
        owner.bind_namespace(&project_meta)?;
        owner.ensure_lease_file()?;
        owner.repair_index()?;
        Ok(owner)
    }

    /// Opens an already initialized rewind store for diagnostics only.
    /// Missing state is distinct from an existing store that cannot be
    /// trusted. In particular, this path never creates directories, binds a
    /// namespace, opens the writer lease for writing, or repairs the index.
    pub fn open_existing_read_only(
        project_root: &Path,
        private_store_root: &Path,
        limits: RewindLimits,
    ) -> Result<Option<Self>, WorkspaceError> {
        let project_root = fs::canonicalize(project_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidProject, project_root))?;
        let project_meta = fs::metadata(&project_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidProject, &project_root))?;
        if !project_meta.is_dir() || project_root.parent().is_none() {
            return Err(WorkspaceError::at(ErrorKind::InvalidProject, project_root));
        }
        if !private_store_root.is_absolute() {
            return Err(WorkspaceError::at(
                ErrorKind::InvalidStore,
                private_store_root,
            ));
        }
        match fs::symlink_metadata(private_store_root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(WorkspaceError::at(
                    ErrorKind::InvalidStore,
                    private_store_root,
                ));
            }
            Ok(_) => validate_private_dir(private_store_root)?,
        }
        let store_root = fs::canonicalize(private_store_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, private_store_root))?;
        if store_root.starts_with(&project_root) || project_root.starts_with(&store_root) {
            return Err(WorkspaceError::at(ErrorKind::InvalidStore, store_root));
        }
        for name in ["manifests", "blobs", "transactions", "recovery"] {
            validate_private_dir(&store_root.join(name))?;
        }
        let namespace_path = store_root.join("namespace.json");
        let namespace: Namespace = read_json(&namespace_path, 64 * 1024)?;
        if namespace.version != NAMESPACE_VERSION
            || namespace.project_root != project_root
            || namespace.root_device != project_meta.dev()
            || namespace.root_inode != project_meta.ino()
        {
            return Err(WorkspaceError::at(ErrorKind::InvalidStore, namespace_path));
        }
        let lease_path = store_root.join("writer.lease");
        let lease = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lease_path)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &lease_path))?;
        validate_private_file(&lease, &lease_path)?;
        Ok(Some(Self {
            project_root,
            store_root,
            limits,
        }))
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn recovery_root(&self) -> PathBuf {
        self.store_root.join("recovery")
    }

    pub fn is_restorable_path(&self, path: &Path) -> Result<bool, WorkspaceError> {
        let absolute = if path.is_absolute() {
            match lexical_absolute(path) {
                Ok(path) => path,
                Err(_) => return Ok(false),
            }
        } else {
            match lexical_absolute(&self.project_root.join(path)) {
                Ok(path) => path,
                Err(_) => return Ok(false),
            }
        };
        if absolute == self.project_root
            || !absolute.starts_with(&self.project_root)
            || is_repository_metadata(&absolute)
            || is_build_cache(&absolute)
            || polaris_tools::path_policy::is_denied(&absolute)
        {
            return Ok(false);
        }
        let protected = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets = RegisteredSecrets::from_auth_registry(registered).map_err(|_| ())?;
            Ok::<_, ()>(safe_path_now(&self.project_root, &absolute, &secrets))
        });
        protected
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| WorkspaceError::new(ErrorKind::ProtectionUnavailable))
    }

    pub fn capture(&self, policy: &SandboxPolicy) -> Result<ManifestId, WorkspaceError> {
        self.validate_policy(policy)?;
        let _lease = self.lease()?;
        self.require_no_pending()?;
        if self.legacy_binding_reservation()?.is_some() {
            return Err(WorkspaceError::new(ErrorKind::Busy));
        }
        self.capture_under_lease()
    }

    /// The first explicit resume of an unbound legacy conversation may bind
    /// only while this project has no workspace identity. Check and capture
    /// under the same project lease so two processes cannot both become the
    /// first binder.
    pub fn bind_first_legacy_head(
        &self,
        policy: &SandboxPolicy,
        session_id: &str,
        bind: impl FnOnce(&ManifestId, PendingLegacyBindingPermit<'_>) -> std::io::Result<()>,
    ) -> Result<ManifestId, WorkspaceError> {
        self.validate_policy(policy)?;
        let lease = self.lease()?;
        self.require_no_pending()?;
        let reservation_path = self.store_root.join("legacy-binding-reservation-v1.json");
        match self.legacy_binding_reservation()? {
            Some(reservation) if reservation.session_id == session_id => {}
            Some(_) => return Err(WorkspaceError::new(ErrorKind::Conflict)),
            None => {
                if self.has_workspace_identity()? {
                    return Err(WorkspaceError::new(ErrorKind::Conflict));
                }
                atomic_json(
                    &reservation_path,
                    &LegacyBindingReservationV1 {
                        version: 1,
                        session_id: session_id.into(),
                    },
                )?;
            }
        }
        let manifest = self.capture_under_lease()?;
        if !self.matches_manifest(&manifest)? {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        bind(
            &manifest,
            PendingLegacyBindingPermit {
                session_id: session_id.into(),
                _lease: &lease,
            },
        )
        .map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
        if !self.matches_manifest(&manifest)? {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        fs::remove_file(&reservation_path)
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, &reservation_path))?;
        File::open(&self.store_root)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, &self.store_root))?;
        Ok(manifest)
    }

    fn legacy_binding_reservation(
        &self,
    ) -> Result<Option<LegacyBindingReservationV1>, WorkspaceError> {
        let path = self.store_root.join("legacy-binding-reservation-v1.json");
        if !path.exists() {
            return Ok(None);
        }
        let reservation: LegacyBindingReservationV1 = read_json(&path, 1024)?;
        if reservation.version != 1
            || reservation.session_id.is_empty()
            || reservation.session_id.len() > 128
            || !reservation
                .session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(WorkspaceError::at(ErrorKind::InvalidStore, &path));
        }
        Ok(Some(reservation))
    }

    pub fn pending_legacy_binding_for(&self, session_id: &str) -> Result<bool, WorkspaceError> {
        Ok(self
            .legacy_binding_reservation()?
            .is_some_and(|reservation| reservation.session_id == session_id))
    }

    fn capture_under_lease(&self) -> Result<ManifestId, WorkspaceError> {
        let deadline = Instant::now()
            .checked_add(self.limits.capture_deadline)
            .ok_or_else(|| WorkspaceError::new(ErrorKind::CaptureDeadline))?;
        let snapshot = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            Some(deadline),
        )
        .map_err(map_snapshot_error)?;
        self.persist_snapshot(&snapshot, Some(deadline))
    }

    pub fn has_workspace_identity(&self) -> Result<bool, WorkspaceError> {
        if self.legacy_binding_reservation()?.is_some() {
            return Ok(true);
        }
        let root = self.store_root.join("manifests");
        for entry in
            fs::read_dir(&root).map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &root))?
        {
            let path = entry
                .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
                .path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                return Ok(true);
            }
        }
        self.has_committed_rewind()
    }

    pub fn preview(
        &self,
        expected_post: &ManifestId,
        target_pre: &ManifestId,
    ) -> Result<Preview, WorkspaceError> {
        let expected = self.load_manifest(expected_post)?;
        let target = self.load_manifest(target_pre)?;
        let changes = plan_changes(&expected, &target);
        let mut blocked_reason = self.preview_block(&expected, &target)?;
        if blocked_reason.is_none() && self.has_pending()? {
            blocked_reason = Some(PreviewBlockedReason::PendingTransaction);
        }
        let created = changes
            .iter()
            .filter(|change| change.kind == PreviewChangeKind::Created)
            .count();
        let deleted = changes
            .iter()
            .filter(|change| change.kind == PreviewChangeKind::Deleted)
            .count();
        let modified = changes.len().saturating_sub(created + deleted);
        Ok(Preview {
            changes,
            created,
            modified,
            deleted,
            restorable: blocked_reason.is_none(),
            blocked_reason,
            recovery_root: self.recovery_root(),
        })
    }

    pub fn restore(
        &self,
        expected_post: &ManifestId,
        target_pre: &ManifestId,
    ) -> Result<RestoreOutcome, WorkspaceError> {
        self.restore_inner(expected_post, target_pre, None)
    }

    pub fn restore_for_branch(
        &self,
        expected_post: &ManifestId,
        target_pre: &ManifestId,
        candidate_session_id: &str,
    ) -> Result<RestoreOutcome, WorkspaceError> {
        if !valid_candidate_session_id(candidate_session_id) {
            return Err(WorkspaceError::new(ErrorKind::InvalidCandidateSession));
        }
        self.restore_inner(
            expected_post,
            target_pre,
            Some(candidate_session_id.to_owned()),
        )
    }

    fn restore_inner(
        &self,
        expected_post: &ManifestId,
        target_pre: &ManifestId,
        candidate_session_id: Option<String>,
    ) -> Result<RestoreOutcome, WorkspaceError> {
        let _lease = self.lease()?;
        self.require_no_pending()?;
        let expected = self.load_manifest(expected_post)?;
        let target = self.load_manifest(target_pre)?;
        if let Some(reason) = self.preview_block(&expected, &target)? {
            return Err(WorkspaceError::new(match reason {
                PreviewBlockedReason::ProjectMismatch => ErrorKind::CorruptManifest,
                PreviewBlockedReason::IncompleteCapture => ErrorKind::IncompleteCapture,
                PreviewBlockedReason::MissingBlob => ErrorKind::MissingBlob,
                PreviewBlockedReason::CorruptBlob => ErrorKind::CorruptBlob,
                PreviewBlockedReason::PendingTransaction => ErrorKind::PendingTransaction,
                PreviewBlockedReason::UnsafeTopology => ErrorKind::Conflict,
            }));
        }
        let current = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            None,
        )
        .map_err(map_snapshot_error)?;
        let observed = manifest_from_snapshot(ManifestId::random()?, &current)?;
        let observed_post_manifest = if same_observation(&expected, &observed) {
            None
        } else {
            Some(
                self.rolled_back_observation_manifest(expected_post, &expected, &observed)?
                    .ok_or_else(|| WorkspaceError::new(ErrorKind::Conflict))?,
            )
        };

        let id = TransactionId::random()?;
        let recovery_path = self.recovery_root().join(id.encoded());
        private_dir_all(&recovery_path)?;
        let source_device = fs::metadata(&self.project_root)
            .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
            .dev();
        if fs::metadata(&recovery_path)
            .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
            .dev()
            != source_device
        {
            return Err(WorkspaceError::at(ErrorKind::CrossDevice, recovery_path));
        }
        let changes = plan_changes(&expected, &target);
        let temporary_paths = temporary_paths_for(&changes, &target, id);
        let mut marker = TransactionMarker {
            version: TRANSACTION_VERSION,
            id,
            expected_post: *expected_post,
            observed_post_manifest,
            target_pre: *target_pre,
            candidate_session_id,
            restored_manifest: None,
            rollback_manifest: None,
            recovery_path: recovery_path.clone(),
            temporary_paths,
            phase: TransactionPhase::Prepared,
        };
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets = RegisteredSecrets::from_auth_registry(registered)
                .map_err(|_| WorkspaceError::new(ErrorKind::ProtectionUnavailable))?;
            for change in &changes {
                let path = self.project_root.join(&change.relative_path);
                if secrets.reserves(&path) {
                    return Err(WorkspaceError::at(ErrorKind::Conflict, path));
                }
            }
            self.prepare_recovery(&expected, &target, &changes, &marker)
        })
        .map_err(|_| WorkspaceError::new(ErrorKind::ProtectionUnavailable))??;
        self.write_marker(&marker)?;
        marker.phase = TransactionPhase::Applying;
        self.write_marker(&marker)?;

        let applied = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets = RegisteredSecrets::from_auth_registry(registered)
                .map_err(|_| WorkspaceError::new(ErrorKind::ProtectionUnavailable))?;
            let latest = isolated_workspace::rewind_snapshot(
                &self.project_root,
                &secrets,
                self.limits.snapshot(),
                None,
            )
            .map_err(map_snapshot_error)?;
            let latest = manifest_from_snapshot(ManifestId::random()?, &latest)?;
            if !same_observation(&observed, &latest) {
                return Err(WorkspaceError::new(ErrorKind::Conflict));
            }
            self.apply_manifest(&observed, &target, &changes, &marker, &secrets)
        });
        applied.map_err(|_| WorkspaceError::new(ErrorKind::ProtectionUnavailable))??;

        let restored = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            None,
        )
        .map_err(map_snapshot_error)?;
        let restored_observation = manifest_from_snapshot(ManifestId::random()?, &restored)?;
        if !same_value(&target, &restored_observation) {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        let restored_id = self.persist_snapshot(&restored, None)?;
        marker.restored_manifest = Some(restored_id);
        marker.phase = TransactionPhase::PendingActivation;
        self.write_marker(&marker)?;
        Ok(RestoreOutcome {
            transaction_id: id,
            restored_manifest_id: restored_id,
            recovery_path,
        })
    }

    pub fn commit(&self, transaction_id: &TransactionId) -> Result<(), WorkspaceError> {
        let lease = self.lease()?;
        let write_result = self.commit_with_lease(&lease, transaction_id);
        match self.transaction_readback_with_lease(&lease, transaction_id) {
            Ok(readback) if readback.phase == RewindTransactionPhase::Committed => Ok(()),
            Err(error) => Err(error),
            Ok(_) => write_result.and(Err(WorkspaceError::new(ErrorKind::InvalidTransaction))),
        }
    }

    /// Commits while the caller retains the project mutation lease across its
    /// own late blocker check and effect record. A write error is never used
    /// to infer rollback eligibility: the marker is durably re-read first.
    pub fn commit_with_lease(
        &self,
        lease: &ProjectLease,
        transaction_id: &TransactionId,
    ) -> Result<(), WorkspaceError> {
        self.validate_lease(lease)?;
        self.commit_locked(transaction_id)
    }

    fn commit_locked(&self, transaction_id: &TransactionId) -> Result<(), WorkspaceError> {
        let mut marker = self.load_marker(transaction_id)?;
        if marker.phase != TransactionPhase::PendingActivation || marker.restored_manifest.is_none()
        {
            return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
        }
        let expected = self.observed_post_manifest(&marker)?;
        let target = self.load_manifest(&marker.target_pre)?;
        let target_map = target.entries_map();
        for entry in &expected.entries {
            let removed = target_map.get(&entry.path).is_none_or(|after| {
                entry.kind != after.kind
                    || (entry.kind == EntryKind::File && !entry.same_value(after))
            });
            if removed
                && !quarantined_entry_matches(
                    &marker
                        .recovery_path
                        .join("removed-forward")
                        .join(recovery_entry_name(&entry.path)),
                    entry,
                )
            {
                return Err(WorkspaceError::at(
                    ErrorKind::Conflict,
                    self.project_root.join(&entry.path),
                ));
            }
        }
        let restored = self.load_manifest(&marker.restored_manifest.unwrap())?;
        let current = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            None,
        )
        .map_err(map_snapshot_error)?;
        let observed = manifest_from_snapshot(ManifestId::random()?, &current)?;
        if !same_observation(&restored, &observed) {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        marker.phase = TransactionPhase::Committed;
        self.write_marker(&marker)
    }

    pub fn rollback(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<RollbackOutcome, WorkspaceError> {
        let lease = self.lease()?;
        let write_result = self.rollback_with_lease(&lease, transaction_id);
        match self.transaction_readback_with_lease(&lease, transaction_id) {
            Ok(readback) if readback.phase == RewindTransactionPhase::RolledBack => {
                let workspace_manifest_id = readback
                    .rollback_manifest_id
                    .ok_or_else(|| WorkspaceError::new(ErrorKind::InvalidTransaction))?;
                Ok(RollbackOutcome {
                    workspace_manifest_id,
                    recovery_path: self.recovery_root().join(transaction_id.encoded()),
                })
            }
            Err(error) => Err(error),
            Ok(_) => write_result.and(Err(WorkspaceError::new(ErrorKind::InvalidTransaction))),
        }
    }

    /// Rolls back only a nonterminal transaction while the caller retains its
    /// project mutation lease. If a write reports an error, a durable readback
    /// can still prove that rollback completed; a committed marker is never
    /// rolled back by this API.
    pub fn rollback_with_lease(
        &self,
        lease: &ProjectLease,
        transaction_id: &TransactionId,
    ) -> Result<RollbackOutcome, WorkspaceError> {
        self.validate_lease(lease)?;
        self.rollback_locked(transaction_id, false)
    }

    /// Re-reads transaction state with file and transaction-directory syncs.
    /// Terminal phases additionally prove the current workspace still equals
    /// the manifest that phase promises.
    pub fn transaction_readback_with_lease(
        &self,
        lease: &ProjectLease,
        transaction_id: &TransactionId,
    ) -> Result<RewindTransactionReadback, WorkspaceError> {
        self.validate_lease(lease)?;
        // Inspect first so a caller that hit a write error can determine
        // whether the marker crossed a terminal phase before the durability
        // barrier. A second read follows the barrier before classification.
        let _before_barrier = self.load_marker(transaction_id)?;
        self.sync_transaction_marker(transaction_id)?;
        let marker = self.load_marker(transaction_id)?;
        let _ = self.observed_post_manifest(&marker)?;
        match marker.phase {
            TransactionPhase::Committed => {
                let restored = marker
                    .restored_manifest
                    .ok_or_else(|| WorkspaceError::new(ErrorKind::InvalidTransaction))?;
                let restored = self.load_manifest(&restored)?;
                let target = self.load_manifest(&marker.target_pre)?;
                if !same_value(&restored, &target) || !self.matches_manifest(&marker.target_pre)? {
                    return Err(WorkspaceError::new(ErrorKind::Conflict));
                }
            }
            TransactionPhase::RolledBack => {
                let rolled = marker
                    .rollback_manifest
                    .ok_or_else(|| WorkspaceError::new(ErrorKind::InvalidTransaction))?;
                let rolled = self.load_manifest(&rolled)?;
                let expected = self.load_manifest(&marker.expected_post)?;
                if !same_value(&rolled, &expected)
                    || !self.matches_manifest(&marker.expected_post)?
                {
                    return Err(WorkspaceError::new(ErrorKind::Conflict));
                }
            }
            _ => {}
        }
        Ok(RewindTransactionReadback {
            transaction_id: marker.id,
            candidate_session_id: marker.candidate_session_id,
            expected_post: marker.expected_post,
            target_pre: marker.target_pre,
            restored_manifest_id: marker.restored_manifest,
            rollback_manifest_id: marker.rollback_manifest,
            phase: marker.phase.into(),
        })
    }

    pub fn resolve_pending(&self) -> Result<ResolveOutcome, WorkspaceError> {
        let _lease = self.lease()?;
        let mut rolled_back = Vec::new();
        for id in self.pending_ids()? {
            rolled_back.push(self.rollback_locked(&id, true)?);
        }
        if rolled_back.is_empty() {
            Ok(ResolveOutcome::Clean)
        } else {
            Ok(ResolveOutcome::RolledBack(rolled_back))
        }
    }

    pub fn has_committed_rewind(&self) -> Result<bool, WorkspaceError> {
        for entry in fs::read_dir(self.store_root.join("transactions"))
            .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
        {
            let path = entry
                .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
                .path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let marker: TransactionMarker = read_json(&path, 1024 * 1024)?;
            if marker.version != TRANSACTION_VERSION {
                return Err(WorkspaceError::at(ErrorKind::InvalidTransaction, path));
            }
            if marker.phase == TransactionPhase::Committed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A successful rollback may restore the original bytes with new inode and
    /// timestamps. Accept that exact persisted observation for a new attempt,
    /// while continuing to reject an external same-value replacement.
    fn rolled_back_observation_manifest(
        &self,
        expected_post: &ManifestId,
        expected: &StoredManifest,
        observed: &StoredManifest,
    ) -> Result<Option<ManifestId>, WorkspaceError> {
        let transactions = self.store_root.join("transactions");
        let mut matched = None;
        for entry in fs::read_dir(&transactions)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &transactions))?
        {
            let path = entry
                .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
                .path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let id: TransactionId = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?
                .parse()
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            let marker = self.load_marker(&id)?;
            if marker.phase != TransactionPhase::RolledBack
                || marker.expected_post != *expected_post
            {
                continue;
            }
            let rolled_id = marker
                .rollback_manifest
                .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            let rolled = self.load_manifest(&rolled_id)?;
            if same_value(expected, &rolled) && same_observation(&rolled, observed) {
                matched = Some(rolled_id);
            }
        }
        Ok(matched)
    }

    fn observed_post_manifest(
        &self,
        marker: &TransactionMarker,
    ) -> Result<StoredManifest, WorkspaceError> {
        self.observed_post_manifest_bounded(marker, &mut None)
    }
    fn observed_post_manifest_bounded(
        &self,
        marker: &TransactionMarker,
        reads: &mut Option<&mut crate::rewind_admission::PublicationReadBudgetV1>,
    ) -> Result<StoredManifest, WorkspaceError> {
        let expected = self.load_manifest_bounded(&marker.expected_post, reads)?;
        let Some(observed_id) = marker.observed_post_manifest else {
            return Ok(expected);
        };
        let observed = self.load_manifest_bounded(&observed_id, reads)?;
        if !same_value(&expected, &observed) {
            return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
        }
        Ok(observed)
    }

    /// Finds the sole committed workspace transaction for a branch candidate.
    /// This is transaction evidence only; callers must separately validate
    /// conversation provenance and the branch raw-event head.
    pub fn committed_branch_transaction(
        &self,
        candidate_session_id: &str,
        expected_post: &ManifestId,
        target_pre: &ManifestId,
    ) -> Result<RewindBranchTransactionEvidence, WorkspaceError> {
        self.committed_branch_transaction_bounded(
            candidate_session_id,
            expected_post,
            target_pre,
            &mut None,
        )
    }

    pub(crate) fn committed_branch_transaction_bounded(
        &self,
        candidate_session_id: &str,
        expected_post: &ManifestId,
        target_pre: &ManifestId,
        reads: &mut Option<&mut crate::rewind_admission::PublicationReadBudgetV1>,
    ) -> Result<RewindBranchTransactionEvidence, WorkspaceError> {
        if !valid_candidate_session_id(candidate_session_id) {
            return Err(WorkspaceError::new(ErrorKind::InvalidCandidateSession));
        }
        let mut matching = None;
        let transactions = self.store_root.join("transactions");
        let paths = match reads.as_deref_mut() {
            Some(reads) => reads
                .paths(&transactions)
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &transactions))?,
            None => fs::read_dir(&transactions)
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &transactions))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|_| WorkspaceError::new(ErrorKind::Io))?,
        };
        for path in paths {
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let id: TransactionId = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?
                .parse()
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            let marker = self.load_marker_bounded(&id, reads)?;
            if marker.candidate_session_id.as_deref() != Some(candidate_session_id) {
                continue;
            }
            if matching.replace(marker).is_some() {
                return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
            }
        }
        let marker = matching.ok_or_else(|| WorkspaceError::new(ErrorKind::InvalidTransaction))?;
        if marker.phase != TransactionPhase::Committed
            || marker.expected_post != *expected_post
            || marker.target_pre != *target_pre
        {
            return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
        }
        let _ = self.observed_post_manifest_bounded(&marker, reads)?;
        let restored_id = marker
            .restored_manifest
            .ok_or_else(|| WorkspaceError::new(ErrorKind::InvalidTransaction))?;
        let restored = self.load_manifest_bounded(&restored_id, reads)?;
        let target = self.load_manifest_bounded(target_pre, reads)?;
        if !same_value(&restored, &target) {
            return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
        }
        Ok(RewindBranchTransactionEvidence {
            transaction_id: marker.id,
            restored_manifest_id: restored_id,
        })
    }

    /// Returns whether exactly one rewind transaction names this candidate.
    /// Admission callers use this to hide a child whose metadata disappeared;
    /// duplicate or malformed transaction records fail closed as an error.
    pub fn has_branch_transaction_candidate(
        &self,
        candidate_session_id: &str,
    ) -> Result<bool, WorkspaceError> {
        self.has_branch_transaction_candidate_bounded(candidate_session_id, &mut None)
    }

    pub(crate) fn has_branch_transaction_candidate_bounded(
        &self,
        candidate_session_id: &str,
        reads: &mut Option<&mut crate::rewind_admission::PublicationReadBudgetV1>,
    ) -> Result<bool, WorkspaceError> {
        if !valid_candidate_session_id(candidate_session_id) {
            return Err(WorkspaceError::new(ErrorKind::InvalidCandidateSession));
        }
        let mut found = false;
        let transactions = self.store_root.join("transactions");
        let paths = match reads.as_deref_mut() {
            Some(reads) => reads
                .paths(&transactions)
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &transactions))?,
            None => fs::read_dir(&transactions)
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &transactions))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|_| WorkspaceError::new(ErrorKind::Io))?,
        };
        for path in paths {
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let id: TransactionId = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?
                .parse()
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            if self
                .load_marker_bounded(&id, reads)?
                .candidate_session_id
                .as_deref()
                == Some(candidate_session_id)
            {
                if found {
                    return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
                }
                found = true;
            }
        }
        Ok(found)
    }

    /// Lists every candidate named by a rewind transaction for project-level
    /// capacity accounting and diagnostics. Marker corruption or duplicate
    /// candidate IDs fail closed instead of being silently skipped.
    pub fn branch_transaction_candidate_ids(&self) -> Result<Vec<String>, WorkspaceError> {
        let transactions = self.store_root.join("transactions");
        let mut candidates = BTreeSet::new();
        for entry in fs::read_dir(&transactions)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &transactions))?
        {
            let path = entry
                .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
                .path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let id: TransactionId = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?
                .parse()
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            if let Some(candidate) = self.load_marker(&id)?.candidate_session_id {
                if !candidates.insert(candidate) {
                    return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
                }
            }
        }
        Ok(candidates.into_iter().collect())
    }

    pub fn matches_manifest(&self, manifest_id: &ManifestId) -> Result<bool, WorkspaceError> {
        let expected = self.load_manifest(manifest_id)?;
        let current = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            None,
        )
        .map_err(map_snapshot_error)?;
        let observed = manifest_from_snapshot(ManifestId::random()?, &current)?;
        Ok(same_value(&expected, &observed))
    }

    fn rollback_locked(
        &self,
        transaction_id: &TransactionId,
        startup: bool,
    ) -> Result<RollbackOutcome, WorkspaceError> {
        let mut marker = self.load_marker(transaction_id)?;
        if matches!(
            marker.phase,
            TransactionPhase::Committed | TransactionPhase::RolledBack
        ) {
            return Err(WorkspaceError::new(ErrorKind::InvalidTransaction));
        }
        let expected = self.observed_post_manifest(&marker)?;
        let target = self.load_manifest(&marker.target_pre)?;
        let current = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            None,
        )
        .map_err(map_snapshot_error)?;
        let observed = manifest_from_snapshot(ManifestId::random()?, &current)?;
        if startup && !known_partial_state(&expected, &target, &observed, &marker) {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        if !startup
            && marker
                .restored_manifest
                .map(|id| self.load_manifest(&id))
                .transpose()?
                .as_ref()
                .is_some_and(|restored| !same_observation(restored, &observed))
        {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        marker.phase = TransactionPhase::RollingBack;
        self.write_marker(&marker)?;
        self.cleanup_temporaries(&marker)?;
        let changes = plan_changes(&observed, &expected);
        marker.temporary_paths = temporary_paths_for(&changes, &expected, marker.id);
        self.write_marker(&marker)?;
        let applied = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets = RegisteredSecrets::from_auth_registry(registered)
                .map_err(|_| WorkspaceError::new(ErrorKind::ProtectionUnavailable))?;
            let latest = isolated_workspace::rewind_snapshot(
                &self.project_root,
                &secrets,
                self.limits.snapshot(),
                None,
            )
            .map_err(map_snapshot_error)?;
            let latest = manifest_from_snapshot(ManifestId::random()?, &latest)?;
            if !same_observation(&observed, &latest) {
                return Err(WorkspaceError::new(ErrorKind::Conflict));
            }
            self.apply_manifest(&observed, &expected, &changes, &marker, &secrets)
        });
        applied.map_err(|_| WorkspaceError::new(ErrorKind::ProtectionUnavailable))??;
        let rolled = isolated_workspace::protected_rewind_snapshot(
            &self.project_root,
            self.limits.snapshot(),
            None,
        )
        .map_err(map_snapshot_error)?;
        let rolled_observation = manifest_from_snapshot(ManifestId::random()?, &rolled)?;
        if !same_value(&expected, &rolled_observation) {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        let workspace_manifest_id = self.persist_snapshot(&rolled, None)?;
        marker.rollback_manifest = Some(workspace_manifest_id);
        marker.phase = TransactionPhase::RolledBack;
        self.write_marker(&marker)?;
        Ok(RollbackOutcome {
            workspace_manifest_id,
            recovery_path: marker.recovery_path,
        })
    }

    fn validate_policy(&self, policy: &SandboxPolicy) -> Result<(), WorkspaceError> {
        if policy.mode() != SandboxMode::WorkspaceWrite
            || !policy.writable_roots().contains(&self.project_root)
        {
            return Err(WorkspaceError::new(ErrorKind::IneligiblePolicy));
        }
        let tmp = fs::canonicalize(std::env::temp_dir())
            .map_err(|_| WorkspaceError::new(ErrorKind::IneligiblePolicy))?;
        if policy
            .writable_roots()
            .iter()
            .any(|root| root != &self.project_root && root != &tmp)
        {
            return Err(WorkspaceError::new(ErrorKind::IneligiblePolicy));
        }
        Ok(())
    }

    fn bind_namespace(&self, project_meta: &Metadata) -> Result<(), WorkspaceError> {
        let path = self.store_root.join("namespace.json");
        let namespace = Namespace {
            version: NAMESPACE_VERSION,
            project_root: self.project_root.clone(),
            root_device: project_meta.dev(),
            root_inode: project_meta.ino(),
        };
        if path.exists() {
            let existing: Namespace = read_json(&path, 64 * 1024)?;
            if existing.version != NAMESPACE_VERSION
                || existing.project_root != namespace.project_root
                || existing.root_device != namespace.root_device
                || existing.root_inode != namespace.root_inode
            {
                return Err(WorkspaceError::at(ErrorKind::InvalidStore, path));
            }
            return Ok(());
        }
        atomic_json(&path, &namespace)
    }

    fn ensure_lease_file(&self) -> Result<(), WorkspaceError> {
        crate::project_mutation::ProjectMutationOwnerV1::at_store_root(&self.store_root)
            .map(|_| ())
            .map_err(project_mutation_error)
    }

    fn lease(&self) -> Result<ProjectLease, WorkspaceError> {
        crate::project_mutation::ProjectMutationOwnerV1::at_store_root(&self.store_root)
            .map_err(project_mutation_error)?
            .try_mutation()
            .map_err(project_mutation_error)
    }

    fn validate_lease(&self, lease: &ProjectLease) -> Result<(), WorkspaceError> {
        if lease.store_root() != self.store_root {
            return Err(WorkspaceError::new(ErrorKind::InvalidStore));
        }
        let root = fs::metadata(&self.store_root)
            .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &self.store_root))?;
        if lease.store_identity() != (root.dev(), root.ino()) {
            return Err(WorkspaceError::new(ErrorKind::InvalidStore));
        }
        Ok(())
    }

    fn sync_transaction_marker(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<(), WorkspaceError> {
        let marker_path = self.marker_path(transaction_id);
        let marker = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker_path)
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, &marker_path))?;
        validate_private_file(&marker, &marker_path)?;
        marker
            .sync_all()
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, &marker_path))?;
        let transactions = self.store_root.join("transactions");
        open_directory_nofollow(&transactions)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, transactions))
    }

    /// Trusted host entry for another project-scoped management owner. The
    /// lease remains tied to this rewind store's original lock file.
    pub fn try_mutation(
        &self,
    ) -> Result<crate::project_mutation::ProjectMutationLeaseV1, WorkspaceError> {
        self.lease()
    }

    /// Check the rewind transaction marker while the caller holds the
    /// project mutation lease used for its own management effect.
    pub fn require_quiescent_with_lease(
        &self,
        lease: &crate::project_mutation::ProjectMutationLeaseV1,
    ) -> Result<(), WorkspaceError> {
        if lease.store_root() != self.store_root {
            return Err(WorkspaceError::new(ErrorKind::InvalidStore));
        }
        self.require_no_pending()
    }

    /// A live root turn or owned child holds the shared side of the same
    /// project lock that protects rewind and other workspace mutations.
    pub fn try_activity(
        &self,
    ) -> Result<crate::project_mutation::ProjectActivityLeaseV1, WorkspaceError> {
        crate::project_mutation::ProjectMutationOwnerV1::at_store_root(&self.store_root)
            .map_err(project_mutation_error)?
            .try_activity()
            .map_err(project_mutation_error)
    }

    /// An execution activity checks unresolved rewind evidence while holding
    /// the shared project lock, closing the gap after startup recovery.
    pub fn try_activity_quiescent(
        &self,
    ) -> Result<crate::project_mutation::ProjectActivityLeaseV1, WorkspaceError> {
        let activity = self.try_activity()?;
        self.require_no_pending()?;
        Ok(activity)
    }

    fn persist_snapshot(
        &self,
        snapshot: &Snapshot,
        deadline: Option<Instant>,
    ) -> Result<ManifestId, WorkspaceError> {
        let id = ManifestId::random()?;
        let manifest = manifest_from_snapshot(id, snapshot)?;
        self.validate_manifest(&manifest)?;
        let existing_bytes = self.blob_bytes()?;
        let mut required = 0u64;
        let mut missing = BTreeMap::new();
        for entry in &manifest.entries {
            if entry.kind != EntryKind::File {
                continue;
            }
            let hash = entry
                .hash
                .ok_or_else(|| WorkspaceError::new(ErrorKind::CorruptManifest))?;
            let path = self.blob_path(hash);
            if !path.exists() && missing.insert(hash, entry.bytes).is_none() {
                required = required
                    .checked_add(entry.bytes)
                    .ok_or_else(|| WorkspaceError::new(ErrorKind::CaptureLimit))?;
            }
        }
        if existing_bytes.saturating_add(required) > self.limits.max_retained_blob_bytes {
            return Err(WorkspaceError::new(ErrorKind::CaptureLimit));
        }
        let mut created = Vec::new();
        let outcome = (|| {
            for entry in &manifest.entries {
                if deadline.is_some_and(|limit| Instant::now() >= limit) {
                    return Err(WorkspaceError::new(ErrorKind::CaptureDeadline));
                }
                if entry.kind != EntryKind::File {
                    continue;
                }
                let hash = entry.hash.unwrap();
                let destination = self.blob_path(hash);
                if destination.exists() {
                    verify_blob(&destination, entry.bytes, hash)?;
                    continue;
                }
                let source = snapshot.path().join(&entry.path);
                copy_blob_atomic(&source, &destination, entry.bytes, hash, deadline)?;
                created.push(destination);
            }
            atomic_json(&self.manifest_path(&id), &manifest)?;
            self.repair_index()?;
            Ok(id)
        })();
        if outcome.is_err() {
            for path in created {
                let _ = fs::remove_file(path);
            }
        }
        outcome
    }

    fn validate_manifest(&self, manifest: &StoredManifest) -> Result<(), WorkspaceError> {
        if manifest.version != MANIFEST_VERSION
            || manifest.project_root != self.project_root
            || manifest
                .entries
                .len()
                .saturating_add(manifest.exclusions.len())
                > self.limits.max_entries
        {
            return Err(WorkspaceError::new(ErrorKind::CorruptManifest));
        }
        let root = fs::metadata(&self.project_root)
            .map_err(|_| WorkspaceError::new(ErrorKind::InvalidProject))?;
        if manifest.root_device != root.dev() || manifest.root_inode != root.ino() {
            return Err(WorkspaceError::new(ErrorKind::Conflict));
        }
        let mut previous: Option<&Path> = None;
        let mut files = 0usize;
        let mut bytes = 0u64;
        for entry in &manifest.entries {
            valid_relative(&entry.path, self.limits.max_depth)?;
            if previous.is_some_and(|path| path >= entry.path.as_path()) {
                return Err(WorkspaceError::new(ErrorKind::CorruptManifest));
            }
            previous = Some(&entry.path);
            match entry.kind {
                EntryKind::File => {
                    files += 1;
                    bytes = bytes
                        .checked_add(entry.bytes)
                        .ok_or_else(|| WorkspaceError::new(ErrorKind::CaptureLimit))?;
                    if files > self.limits.max_files
                        || entry.bytes > self.limits.max_file_bytes
                        || bytes > self.limits.max_manifest_bytes
                        || entry.hash.is_none()
                    {
                        return Err(WorkspaceError::new(ErrorKind::CaptureLimit));
                    }
                }
                EntryKind::Directory if entry.bytes != 0 || entry.hash.is_some() => {
                    return Err(WorkspaceError::new(ErrorKind::CorruptManifest));
                }
                EntryKind::Directory => {}
            }
        }
        for exclusion in &manifest.exclusions {
            valid_relative(&exclusion.path, self.limits.max_depth)?;
        }
        Ok(())
    }

    fn load_manifest(&self, id: &ManifestId) -> Result<StoredManifest, WorkspaceError> {
        self.load_manifest_bounded(id, &mut None)
    }
    fn load_manifest_bounded(
        &self,
        id: &ManifestId,
        reads: &mut Option<&mut crate::rewind_admission::PublicationReadBudgetV1>,
    ) -> Result<StoredManifest, WorkspaceError> {
        let path = self.manifest_path(id);
        if !path.exists() {
            return Err(WorkspaceError::at(ErrorKind::MissingManifest, path));
        }
        let manifest: StoredManifest = match reads.as_deref_mut() {
            Some(reads) => serde_json::from_slice(
                &reads
                    .read(&path)
                    .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &path))?,
            )
            .map_err(|_| WorkspaceError::at(ErrorKind::CorruptManifest, &path))?,
            None => read_json(&path, 32 * 1024 * 1024)?,
        };
        if manifest.id != *id {
            return Err(WorkspaceError::at(ErrorKind::CorruptManifest, path));
        }
        self.validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn manifest_path(&self, id: &ManifestId) -> PathBuf {
        self.store_root
            .join("manifests")
            .join(format!("{}.json", id.encoded()))
    }

    fn blob_path(&self, hash: [u8; 32]) -> PathBuf {
        self.store_root.join("blobs").join(hex(&hash))
    }

    fn marker_path(&self, id: &TransactionId) -> PathBuf {
        self.store_root
            .join("transactions")
            .join(format!("{}.json", id.encoded()))
    }

    fn load_marker(&self, id: &TransactionId) -> Result<TransactionMarker, WorkspaceError> {
        self.load_marker_bounded(id, &mut None)
    }
    fn load_marker_bounded(
        &self,
        id: &TransactionId,
        reads: &mut Option<&mut crate::rewind_admission::PublicationReadBudgetV1>,
    ) -> Result<TransactionMarker, WorkspaceError> {
        let path = self.marker_path(id);
        let marker: TransactionMarker = match reads.as_deref_mut() {
            Some(reads) => serde_json::from_slice(
                &reads
                    .read(&path)
                    .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, &path))?,
            )
            .map_err(|_| WorkspaceError::at(ErrorKind::CorruptManifest, &path))?,
            None => read_json(&path, 1024 * 1024)?,
        };
        if marker.version != TRANSACTION_VERSION
            || marker.id != *id
            || marker.recovery_path != self.recovery_root().join(id.encoded())
            || marker
                .candidate_session_id
                .as_deref()
                .is_some_and(|value| !valid_candidate_session_id(value))
        {
            return Err(WorkspaceError::at(ErrorKind::InvalidTransaction, path));
        }
        Ok(marker)
    }

    fn write_marker(&self, marker: &TransactionMarker) -> Result<(), WorkspaceError> {
        atomic_json(&self.marker_path(&marker.id), marker)
    }

    fn pending_ids(&self) -> Result<Vec<TransactionId>, WorkspaceError> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.store_root.join("transactions"))
            .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
        {
            let entry = entry.map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            let id: TransactionId = stem
                .parse()
                .map_err(|_| WorkspaceError::at(ErrorKind::InvalidTransaction, &path))?;
            let marker = self.load_marker(&id)?;
            if !matches!(
                marker.phase,
                TransactionPhase::Committed | TransactionPhase::RolledBack
            ) {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn has_pending(&self) -> Result<bool, WorkspaceError> {
        Ok(!self.pending_ids()?.is_empty())
    }

    fn require_no_pending(&self) -> Result<(), WorkspaceError> {
        if self.has_pending()? {
            Err(WorkspaceError::new(ErrorKind::PendingTransaction))
        } else {
            Ok(())
        }
    }

    fn preview_block(
        &self,
        expected: &StoredManifest,
        target: &StoredManifest,
    ) -> Result<Option<PreviewBlockedReason>, WorkspaceError> {
        if expected.project_root != target.project_root
            || expected.project_root != self.project_root
        {
            return Ok(Some(PreviewBlockedReason::ProjectMismatch));
        }
        if important_exclusions(expected) != important_exclusions(target) {
            return Ok(Some(PreviewBlockedReason::UnsafeTopology));
        }
        if expected.exclusions.iter().any(|entry| {
            entry.reason == StoredExclusionReason::RepositoryMetadata
                && plan_changes(expected, target)
                    .iter()
                    .any(|change| overlaps(&change.relative_path, &entry.path))
        }) {
            return Ok(Some(PreviewBlockedReason::UnsafeTopology));
        }
        for manifest in [expected, target] {
            for entry in &manifest.entries {
                if entry.kind == EntryKind::File {
                    let hash = entry.hash.unwrap();
                    let blob = self.blob_path(hash);
                    if !blob.exists() {
                        return Ok(Some(PreviewBlockedReason::MissingBlob));
                    }
                    if verify_blob(&blob, entry.bytes, hash).is_err() {
                        return Ok(Some(PreviewBlockedReason::CorruptBlob));
                    }
                }
            }
        }
        Ok(None)
    }

    fn blob_bytes(&self) -> Result<u64, WorkspaceError> {
        let mut total = 0u64;
        for entry in fs::read_dir(self.store_root.join("blobs"))
            .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
        {
            let entry = entry.map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
            let metadata = entry
                .metadata()
                .map_err(|_| WorkspaceError::at(ErrorKind::Io, entry.path()))?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(WorkspaceError::at(ErrorKind::InvalidStore, entry.path()));
            }
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| WorkspaceError::new(ErrorKind::CaptureLimit))?;
        }
        Ok(total)
    }

    fn repair_index(&self) -> Result<(), WorkspaceError> {
        let mut references: BTreeMap<String, u64> = BTreeMap::new();
        for entry in fs::read_dir(self.store_root.join("manifests"))
            .map_err(|_| WorkspaceError::new(ErrorKind::Io))?
        {
            let entry = entry.map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let manifest: StoredManifest = read_json(&path, 32 * 1024 * 1024)?;
            self.validate_manifest(&manifest)?;
            for file in manifest
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::File)
            {
                *references.entry(hex(&file.hash.unwrap())).or_default() += 1;
            }
        }
        atomic_json(&self.store_root.join("blob-index.json"), &references)
    }

    fn prepare_recovery(
        &self,
        expected: &StoredManifest,
        target: &StoredManifest,
        changes: &[PreviewChange],
        marker: &TransactionMarker,
    ) -> Result<(), WorkspaceError> {
        let originals = marker.recovery_path.join("originals");
        let staged = marker.recovery_path.join("staged");
        private_dir_all(&originals)?;
        private_dir_all(&staged)?;
        let expected_map = expected.entries_map();
        let target_map = target.entries_map();
        for change in changes {
            let recovery_name = recovery_entry_name(&change.relative_path);
            if let Some(entry) = expected_map.get(&change.relative_path)
                && entry.kind == EntryKind::File
            {
                copy_blob_atomic(
                    &self.blob_path(entry.hash.unwrap()),
                    &originals.join(&recovery_name),
                    entry.bytes,
                    entry.hash.unwrap(),
                    None,
                )?;
            }
            if let Some(entry) = target_map.get(&change.relative_path)
                && entry.kind == EntryKind::File
            {
                copy_blob_atomic(
                    &self.blob_path(entry.hash.unwrap()),
                    &staged.join(&recovery_name),
                    entry.bytes,
                    entry.hash.unwrap(),
                    None,
                )?;
            }
        }
        atomic_json(
            &marker.recovery_path.join("recovery.json"),
            &serde_json::json!({
                "version": 1,
                "transaction": marker.id,
                "expected_post": marker.expected_post,
                "target_pre": marker.target_pre,
                "candidate_session_id": marker.candidate_session_id,
                "changes": changes,
            }),
        )
    }

    fn apply_manifest(
        &self,
        current: &StoredManifest,
        target: &StoredManifest,
        changes: &[PreviewChange],
        marker: &TransactionMarker,
        secrets: &RegisteredSecrets,
    ) -> Result<(), WorkspaceError> {
        let current_map = current.entries_map();
        let target_map = target.entries_map();
        for change in changes {
            let absolute = self.project_root.join(&change.relative_path);
            if !safe_path_now(&self.project_root, &absolute, secrets)
                && fs::symlink_metadata(&absolute).is_ok()
            {
                return Err(WorkspaceError::at(ErrorKind::Conflict, absolute));
            }
            guard_ancestors(&self.project_root, &absolute, secrets)?;
        }
        for temporary in &marker.temporary_paths {
            let absolute = self.project_root.join(temporary);
            if fs::symlink_metadata(&absolute).is_ok() {
                return Err(WorkspaceError::at(ErrorKind::Conflict, absolute));
            }
            guard_ancestors(&self.project_root, &absolute, secrets)?;
        }

        let mut removals: Vec<_> = changes
            .iter()
            .filter_map(|change| {
                current_map
                    .get(&change.relative_path)
                    .filter(|before| {
                        target_map.get(&change.relative_path).is_none_or(|after| {
                            before.kind != after.kind
                                || (before.kind == EntryKind::File && !before.same_value(after))
                        })
                    })
                    .map(|entry| ((*entry).clone(), change.relative_path.clone()))
            })
            .collect();
        removals.sort_by(|(left, _), (right, _)| {
            depth(&right.path)
                .cmp(&depth(&left.path))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        let removed = marker
            .recovery_path
            .join(if target.id == marker.target_pre {
                "removed-forward"
            } else {
                "removed-rollback"
            });
        private_dir_all(&removed)?;
        for (entry, relative) in removals {
            let path = self.project_root.join(&relative);
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| WorkspaceError::at(ErrorKind::Conflict, &path))?;
            if !matches_entry_metadata(&entry, &metadata) {
                return Err(WorkspaceError::at(ErrorKind::Conflict, path));
            }
            if entry.kind == EntryKind::Directory && !directory_is_empty(&path) {
                return Err(WorkspaceError::at(ErrorKind::Conflict, path));
            }
            #[cfg(test)]
            if entry.kind == EntryKind::Directory {
                invoke_directory_remove_hook(&path);
            }
            #[cfg(test)]
            invoke_remove_hook(&path);
            let held = removed.join(recovery_entry_name(&relative));
            rename_noreplace(&path, &held)
                .map_err(|_| WorkspaceError::at(ErrorKind::RestoreFailed, &path))?;
            sync_parent(&path)?;
            sync_parent(&held)?;
            if !quarantined_entry_matches(&held, &entry) {
                // Another editor replaced or edited the path during apply.
                // Restore the raced object only if its old name is still
                // vacant; otherwise retain it under this recovery record.
                let _ = rename_noreplace(&held, &path);
                return Err(WorkspaceError::at(ErrorKind::Conflict, path));
            }
        }

        let mut directories: Vec<_> = target
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .collect();
        directories.sort_by_key(|entry| depth(&entry.path));
        for entry in &directories {
            let path = self.project_root.join(&entry.path);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => return Err(WorkspaceError::at(ErrorKind::Conflict, path)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_directory_nofollow(&path, 0o700)
                        .map_err(|_| WorkspaceError::at(ErrorKind::RestoreFailed, &path))?;
                    sync_parent(&path)?;
                }
                Err(_) => return Err(WorkspaceError::at(ErrorKind::Io, path)),
            }
        }

        for change in changes {
            let Some(entry) = target_map.get(&change.relative_path) else {
                continue;
            };
            if entry.kind != EntryKind::File {
                continue;
            }
            let destination = self.project_root.join(&entry.path);
            let stage = if target.id == marker.target_pre {
                "staged"
            } else {
                "originals"
            };
            let recovery_name = recovery_entry_name(&change.relative_path);
            let staged = marker.recovery_path.join(stage).join(&recovery_name);
            let install = marker
                .recovery_path
                .join(format!("install-{stage}-{recovery_name}"));
            copy_blob_atomic(&staged, &install, entry.bytes, entry.hash.unwrap(), None)?;
            fs::set_permissions(&install, Permissions::from_mode(entry.mode & 0o777))
                .map_err(|_| WorkspaceError::at(ErrorKind::Io, &install))?;
            File::open(&install)
                .and_then(|file| file.sync_all())
                .map_err(|_| WorkspaceError::at(ErrorKind::Io, &install))?;
            if fs::symlink_metadata(&destination).is_ok() {
                return Err(WorkspaceError::at(ErrorKind::Conflict, destination));
            }
            #[cfg(test)]
            invoke_install_hook(&destination);
            rename_noreplace(&install, &destination).map_err(|error| {
                WorkspaceError::at(
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        ErrorKind::Conflict
                    } else {
                        ErrorKind::RestoreFailed
                    },
                    &destination,
                )
            })?;
            sync_parent(&destination)?;
        }

        directories.sort_by_key(|entry| std::cmp::Reverse(depth(&entry.path)));
        for entry in directories {
            let path = self.project_root.join(&entry.path);
            let directory = open_directory_nofollow(&path)
                .map_err(|_| WorkspaceError::at(ErrorKind::Conflict, &path))?;
            if unsafe { libc::fchmod(directory.as_raw_fd(), (entry.mode & 0o777) as libc::mode_t) }
                != 0
            {
                return Err(WorkspaceError::at(ErrorKind::RestoreFailed, &path));
            }
            directory
                .sync_all()
                .map_err(|_| WorkspaceError::at(ErrorKind::Io, &path))?;
        }
        Ok(())
    }

    fn cleanup_temporaries(&self, marker: &TransactionMarker) -> Result<(), WorkspaceError> {
        for relative in &marker.temporary_paths {
            let path = self.project_root.join(relative);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && metadata.nlink() == 1 => {
                    remove_file_nofollow(&path)
                        .map_err(|_| WorkspaceError::at(ErrorKind::Io, &path))?;
                }
                Ok(_) => return Err(WorkspaceError::at(ErrorKind::Conflict, path)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(WorkspaceError::at(ErrorKind::Io, path)),
            }
        }
        Ok(())
    }
}

impl StoredManifest {
    fn entries_map(&self) -> BTreeMap<PathBuf, &StoredEntry> {
        self.entries
            .iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect()
    }
}

type ProjectLease = crate::project_mutation::ProjectMutationLeaseV1;

fn project_mutation_error(error: crate::project_mutation::ProjectMutationError) -> WorkspaceError {
    match error {
        crate::project_mutation::ProjectMutationError::Busy => WorkspaceError::new(ErrorKind::Busy),
        crate::project_mutation::ProjectMutationError::InvalidStore(path) => {
            WorkspaceError::at(ErrorKind::InvalidStore, path)
        }
        crate::project_mutation::ProjectMutationError::Io(_) => {
            WorkspaceError::new(ErrorKind::InvalidStore)
        }
    }
}

fn manifest_from_snapshot(
    id: ManifestId,
    snapshot: &Snapshot,
) -> Result<StoredManifest, WorkspaceError> {
    let root = fs::metadata(&snapshot.source_path)
        .map_err(|_| WorkspaceError::at(ErrorKind::InvalidProject, &snapshot.source_path))?;
    let mut exclusions = Vec::new();
    for exclusion in &snapshot.exclusions {
        let reason = match exclusion.reason {
            ExclusionReason::RepositoryMetadata => StoredExclusionReason::RepositoryMetadata,
            ExclusionReason::BuildCache => StoredExclusionReason::BuildCache,
            _ => {
                return Err(WorkspaceError::at(
                    ErrorKind::IncompleteCapture,
                    &exclusion.relative_path,
                ));
            }
        };
        exclusions.push(StoredExclusion {
            path: exclusion.relative_path.clone(),
            reason,
        });
    }
    exclusions.sort_by(|left, right| left.path.cmp(&right.path));
    let mut entries = snapshot
        .manifest
        .iter()
        .map(|entry| StoredEntry {
            path: entry.relative_path.clone(),
            kind: if entry.sha256.is_some() {
                EntryKind::File
            } else {
                EntryKind::Directory
            },
            mode: entry.mode,
            bytes: entry.bytes,
            hash: entry.sha256,
            device: entry.identity.device,
            inode: entry.identity.inode,
            mtime: entry.observed_mtime(),
            ctime: entry.observed_ctime(),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(StoredManifest {
        version: MANIFEST_VERSION,
        id,
        project_root: snapshot.source_path.clone(),
        root_device: root.dev(),
        root_inode: root.ino(),
        captured_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis()),
        entries,
        exclusions,
    })
}

fn plan_changes(before: &StoredManifest, after: &StoredManifest) -> Vec<PreviewChange> {
    let before = before.entries_map();
    let after = after.entries_map();
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let kind = match (before.get(&path), after.get(&path)) {
                (None, Some(_)) => PreviewChangeKind::Created,
                (Some(_), None) => PreviewChangeKind::Deleted,
                (Some(left), Some(right)) if left.same_value(right) => return None,
                (Some(left), Some(right)) if left.kind != right.kind => {
                    PreviewChangeKind::TypeChanged
                }
                (Some(left), Some(right))
                    if left.hash == right.hash && left.bytes == right.bytes =>
                {
                    PreviewChangeKind::ModeChanged
                }
                (Some(_), Some(_)) => PreviewChangeKind::Modified,
                (None, None) => return None,
            };
            Some(PreviewChange {
                relative_path: path,
                kind,
            })
        })
        .collect()
}

fn same_value(left: &StoredManifest, right: &StoredManifest) -> bool {
    left.project_root == right.project_root
        && left.root_device == right.root_device
        && left.root_inode == right.root_inode
        && left.entries.len() == right.entries.len()
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| left.same_value(right))
        && important_exclusions(left) == important_exclusions(right)
}

fn same_observation(left: &StoredManifest, right: &StoredManifest) -> bool {
    same_value(left, right)
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| left.same_observation(right))
}

fn known_partial_state(
    expected: &StoredManifest,
    target: &StoredManifest,
    current: &StoredManifest,
    marker: &TransactionMarker,
) -> bool {
    if expected.project_root != current.project_root
        || expected.root_device != current.root_device
        || expected.root_inode != current.root_inode
        || important_exclusions(expected) != important_exclusions(current)
    {
        return false;
    }
    let expected_map = expected.entries_map();
    let target_map = target.entries_map();
    let current_map = current.entries_map();
    let changed_paths: Vec<_> = plan_changes(expected, target)
        .into_iter()
        .map(|change| change.relative_path)
        .collect();
    let paths: BTreeSet<_> = expected_map
        .keys()
        .chain(target_map.keys())
        .chain(current_map.keys())
        .cloned()
        .collect();
    paths.into_iter().all(|path| {
        let before = expected_map.get(&path).copied();
        let after = target_map.get(&path).copied();
        let now = current_map.get(&path).copied();
        let directly_changed = before
            .zip(after)
            .is_none_or(|(left, right)| !left.same_value(right));
        if directly_changed {
            return match now {
                None => match marker.phase {
                    TransactionPhase::Prepared => before.is_none(),
                    TransactionPhase::Applying | TransactionPhase::PendingActivation => before
                        .is_none_or(|entry| {
                            quarantined_entry_matches(
                                &marker
                                    .recovery_path
                                    .join("removed-forward")
                                    .join(recovery_entry_name(&path)),
                                entry,
                            )
                        }),
                    TransactionPhase::RollingBack => {
                        if let Some(entry) = after {
                            quarantined_entry_matches(
                                &marker
                                    .recovery_path
                                    .join("removed-rollback")
                                    .join(recovery_entry_name(&path)),
                                entry,
                            )
                        } else {
                            before.is_none_or(|entry| {
                                quarantined_entry_matches(
                                    &marker
                                        .recovery_path
                                        .join("removed-forward")
                                        .join(recovery_entry_name(&path)),
                                    entry,
                                )
                            })
                        }
                    }
                    TransactionPhase::Committed | TransactionPhase::RolledBack => false,
                },
                Some(now) if before.is_some_and(|entry| entry.same_value(now)) => {
                    before.is_some_and(|entry| entry.same_observation(now))
                }
                Some(now) => after.is_some_and(|entry| entry.same_value(now)),
            };
        }
        let Some(before) = before else {
            return now.is_none();
        };
        let Some(now) = now else { return false };
        if before.kind == EntryKind::Directory
            && changed_paths
                .iter()
                .any(|changed| changed.starts_with(&path))
        {
            before.same_value(now) && before.device == now.device && before.inode == now.inode
        } else {
            before.same_observation(now)
        }
    })
}

fn important_exclusions(manifest: &StoredManifest) -> Vec<&Path> {
    manifest
        .exclusions
        .iter()
        .filter(|entry| entry.reason == StoredExclusionReason::RepositoryMetadata)
        .map(|entry| entry.path.as_path())
        .collect()
}

fn temporary_paths_for(
    _changes: &[PreviewChange],
    _target: &StoredManifest,
    _id: TransactionId,
) -> Vec<PathBuf> {
    Vec::new()
}

fn map_snapshot_error(error: isolated_workspace::SnapshotError) -> WorkspaceError {
    let kind = match error.kind {
        isolated_workspace::ErrorKind::Deadline => ErrorKind::CaptureDeadline,
        isolated_workspace::ErrorKind::Limit => ErrorKind::CaptureLimit,
        isolated_workspace::ErrorKind::ProtectionUnavailable => ErrorKind::ProtectionUnavailable,
        isolated_workspace::ErrorKind::SourceChanged => ErrorKind::Conflict,
        _ => ErrorKind::Io,
    };
    WorkspaceError::at(kind, error.path)
}

fn valid_relative(path: &Path, max_depth: usize) -> Result<(), WorkspaceError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.components().count().saturating_sub(1) > max_depth
    {
        return Err(WorkspaceError::at(ErrorKind::CorruptManifest, path));
    }
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(WorkspaceError::at(ErrorKind::InvalidProject, path));
    }
    Ok(path.to_path_buf())
}

fn is_repository_metadata(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(b".git")
    })
}

fn is_build_cache(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("target" | "node_modules" | ".build" | ".cache" | "DerivedData")
        )
    })
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn depth(path: &Path) -> usize {
    path.components().count()
}

fn recovery_entry_name(path: &Path) -> String {
    hex(&Sha256::digest(path.as_os_str().as_encoded_bytes()))
}

fn valid_candidate_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn safe_path_now(project_root: &Path, absolute: &Path, secrets: &RegisteredSecrets) -> bool {
    if absolute == project_root
        || !absolute.starts_with(project_root)
        || is_repository_metadata(absolute)
        || is_build_cache(absolute)
        || polaris_tools::path_policy::is_denied(absolute)
        || secrets.reserves(absolute)
    {
        return false;
    }
    let relative = match absolute.strip_prefix(project_root) {
        Ok(relative) => relative,
        Err(_) => return false,
    };
    let mut cursor = project_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || (!metadata.is_dir() && (!metadata.is_file() || metadata.nlink() != 1))
                    || secrets.denies(&cursor, &metadata)
                {
                    return false;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }
    true
}

fn guard_ancestors(
    project_root: &Path,
    absolute: &Path,
    secrets: &RegisteredSecrets,
) -> Result<(), WorkspaceError> {
    let parent = absolute
        .parent()
        .ok_or_else(|| WorkspaceError::at(ErrorKind::Conflict, absolute))?;
    if !parent.starts_with(project_root) {
        return Err(WorkspaceError::at(ErrorKind::Conflict, absolute));
    }
    let relative = parent
        .strip_prefix(project_root)
        .map_err(|_| WorkspaceError::at(ErrorKind::Conflict, parent))?;
    let mut cursor = project_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(WorkspaceError::at(ErrorKind::Conflict, parent));
        };
        cursor.push(name);
        let metadata = fs::symlink_metadata(&cursor)
            .map_err(|_| WorkspaceError::at(ErrorKind::Conflict, &cursor))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || secrets.denies(&cursor, &metadata)
        {
            return Err(WorkspaceError::at(ErrorKind::Conflict, cursor));
        }
    }
    Ok(())
}

fn matches_entry_metadata(entry: &StoredEntry, metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink()
        || metadata.dev() != entry.device
        || metadata.ino() != entry.inode
        || metadata.mode() & 0o777 != entry.mode
    {
        return false;
    }
    match entry.kind {
        EntryKind::File => {
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.len() == entry.bytes
                && (metadata.mtime(), metadata.mtime_nsec()) == entry.mtime
                && (metadata.ctime(), metadata.ctime_nsec()) == entry.ctime
        }
        // Removing our own children changes directory timestamps. Identity and
        // mode still detect replacement while remove_dir rejects extra entries.
        EntryKind::Directory => metadata.is_dir(),
    }
}

fn matches_moved_entry_metadata(entry: &StoredEntry, metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink()
        || metadata.dev() != entry.device
        || metadata.ino() != entry.inode
        || metadata.mode() & 0o777 != entry.mode
    {
        return false;
    }
    match entry.kind {
        EntryKind::File => {
            metadata.is_file() && metadata.nlink() == 1 && metadata.len() == entry.bytes
        }
        EntryKind::Directory => metadata.is_dir(),
    }
}

fn directory_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

fn quarantined_entry_matches(path: &Path, entry: &StoredEntry) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    matches_moved_entry_metadata(entry, &metadata)
        && match entry.kind {
            EntryKind::File => matches_file_hash(path, entry.bytes, entry.hash.unwrap())
                .ok()
                .is_some_and(|matches| matches),
            EntryKind::Directory => directory_is_empty(path),
        }
}

fn matches_file_hash(
    path: &Path,
    expected_bytes: u64,
    expected_hash: [u8; 32],
) -> Result<bool, WorkspaceError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| WorkspaceError::at(ErrorKind::Conflict, path))?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| WorkspaceError::at(ErrorKind::Conflict, path))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| WorkspaceError::at(ErrorKind::Conflict, path))?;
        if total > expected_bytes {
            return Ok(false);
        }
        digest.update(&buffer[..count]);
    }
    Ok(total == expected_bytes && <[u8; 32]>::from(digest.finalize()) == expected_hash)
}

fn private_dir_all(path: &Path) -> Result<(), WorkspaceError> {
    fs::create_dir_all(path).map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, path))?;
    fs::set_permissions(path, Permissions::from_mode(0o700))
        .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, path))?;
    validate_private_dir(path)
}

fn validate_private_dir(path: &Path) -> Result<(), WorkspaceError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, path))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7077 != 0
        || metadata.mode() & 0o700 != 0o700
    {
        return Err(WorkspaceError::at(ErrorKind::InvalidStore, path));
    }
    Ok(())
}

fn validate_private_file(file: &File, path: &Path) -> Result<(), WorkspaceError> {
    let metadata = file
        .metadata()
        .map_err(|_| WorkspaceError::at(ErrorKind::InvalidStore, path))?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7177 != 0
    {
        return Err(WorkspaceError::at(ErrorKind::InvalidStore, path));
    }
    Ok(())
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), WorkspaceError> {
    let parent = path
        .parent()
        .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidStore, path))?;
    validate_private_dir(parent)?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
    let temporary = parent.join(format!(".write-{}.tmp", hex(&random)));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| WorkspaceError::at(ErrorKind::Io, &temporary))?;
    let result = (|| {
        serde_json::to_writer(&mut file, value)
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?;
        file.write_all(b"\n")
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?;
        file.sync_all()
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?;
        fs::rename(&temporary, path).map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, parent))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_bytes: u64,
) -> Result<T, WorkspaceError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?;
    validate_private_file(&file, path)?;
    let length = file
        .metadata()
        .map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?
        .len();
    if length > max_bytes {
        return Err(WorkspaceError::at(ErrorKind::CorruptManifest, path));
    }
    let mut body = Vec::with_capacity(length as usize);
    file.read_to_end(&mut body)
        .map_err(|_| WorkspaceError::at(ErrorKind::Io, path))?;
    serde_json::from_slice(&body).map_err(|_| WorkspaceError::at(ErrorKind::CorruptManifest, path))
}

fn copy_blob_atomic(
    source: &Path,
    destination: &Path,
    expected_bytes: u64,
    expected_hash: [u8; 32],
    deadline: Option<Instant>,
) -> Result<(), WorkspaceError> {
    let parent = destination
        .parent()
        .ok_or_else(|| WorkspaceError::at(ErrorKind::InvalidStore, destination))?;
    validate_private_dir(parent)?;
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| WorkspaceError::new(ErrorKind::Io))?;
    let temporary = parent.join(format!(".blob-{}.tmp", hex(&random)));
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(|_| WorkspaceError::at(ErrorKind::MissingBlob, source))?;
    let source_meta = input
        .metadata()
        .map_err(|_| WorkspaceError::at(ErrorKind::CorruptBlob, source))?;
    if !source_meta.is_file() || source_meta.nlink() != 1 || source_meta.len() != expected_bytes {
        return Err(WorkspaceError::at(ErrorKind::CorruptBlob, source));
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| WorkspaceError::at(ErrorKind::Io, &temporary))?;
    let result = (|| {
        let mut digest = Sha256::new();
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Err(WorkspaceError::new(ErrorKind::CaptureDeadline));
            }
            let read = input
                .read(&mut buffer)
                .map_err(|_| WorkspaceError::at(ErrorKind::Io, source))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > expected_bytes {
                return Err(WorkspaceError::at(ErrorKind::CorruptBlob, source));
            }
            digest.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|_| WorkspaceError::at(ErrorKind::Io, &temporary))?;
        }
        if total != expected_bytes || <[u8; 32]>::from(digest.finalize()) != expected_hash {
            return Err(WorkspaceError::at(ErrorKind::CorruptBlob, source));
        }
        output
            .sync_all()
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, &temporary))?;
        fs::rename(&temporary, destination)
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, destination))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| WorkspaceError::at(ErrorKind::Io, parent))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn verify_blob(
    path: &Path,
    expected_bytes: u64,
    expected_hash: [u8; 32],
) -> Result<(), WorkspaceError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| WorkspaceError::at(ErrorKind::MissingBlob, path))?;
    validate_private_file(&file, path)?;
    let metadata = file
        .metadata()
        .map_err(|_| WorkspaceError::at(ErrorKind::CorruptBlob, path))?;
    if metadata.len() != expected_bytes {
        return Err(WorkspaceError::at(ErrorKind::CorruptBlob, path));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| WorkspaceError::at(ErrorKind::CorruptBlob, path))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| WorkspaceError::at(ErrorKind::CorruptBlob, path))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if <[u8; 32]>::from(digest.finalize()) != expected_hash {
        return Err(WorkspaceError::at(ErrorKind::CorruptBlob, path));
    }
    Ok(())
}

/// Install only when the destination name is still absent. Ordinary rename
/// replaces a file created by another editor after our last metadata check.
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let (source_parent, source_name) = open_parent_nofollow(source)?;
    let (destination_parent, destination_name) = open_parent_nofollow(destination)?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    if !path.is_absolute() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

fn open_parent_nofollow(path: &Path) -> std::io::Result<(File, CString)> {
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    Ok((open_directory_nofollow(parent)?, name))
}

fn create_directory_nofollow(path: &Path, mode: u32) -> std::io::Result<()> {
    let (parent, name) = open_parent_nofollow(path)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_file_nofollow(path: &Path) -> std::io::Result<()> {
    let (parent, name) = open_parent_nofollow(path)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
type PathHook = std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>>;

#[cfg(test)]
thread_local! {
    static INSTALL_HOOK: PathHook = std::cell::RefCell::new(None);
    static REMOVE_HOOK: PathHook = std::cell::RefCell::new(None);
    static DIRECTORY_REMOVE_HOOK: PathHook = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn invoke_install_hook(destination: &Path) {
    INSTALL_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(destination);
        }
    });
}

#[cfg(test)]
fn invoke_remove_hook(path: &Path) {
    REMOVE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(path);
        }
    });
}

#[cfg(test)]
fn invoke_directory_remove_hook(path: &Path) {
    DIRECTORY_REMOVE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(path);
        }
    });
}

fn sync_parent(path: &Path) -> Result<(), WorkspaceError> {
    let parent = path
        .parent()
        .ok_or_else(|| WorkspaceError::at(ErrorKind::Io, path))?;
    open_directory_nofollow(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| WorkspaceError::at(ErrorKind::Io, parent))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, SystemTime, Option<Vec<u8>>)> {
        let mut result = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let bytes = metadata.is_file().then(|| fs::read(&path).unwrap());
            if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            }
            result.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                (metadata.mode(), metadata.modified().unwrap(), bytes),
            );
        }
        result
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        project: PathBuf,
        store: PathBuf,
        owner: WorkspaceOwner,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let project = root.join("project");
            let store = root.join("store");
            fs::create_dir(&project).unwrap();
            let owner = WorkspaceOwner::open(
                &project,
                &store,
                RewindLimits {
                    max_entries: 128,
                    max_files: 64,
                    max_file_bytes: 1024 * 1024,
                    max_manifest_bytes: 8 * 1024 * 1024,
                    max_retained_blob_bytes: 16 * 1024 * 1024,
                    max_depth: 16,
                    capture_deadline: Duration::from_secs(5),
                },
            )
            .unwrap();
            Self {
                _temp: temp,
                project,
                store,
                owner,
            }
        }

        fn policy(&self) -> SandboxPolicy {
            SandboxPolicy::new(
                SandboxMode::WorkspaceWrite,
                std::slice::from_ref(&self.project),
            )
            .unwrap()
        }

        fn transaction_marker(
            &self,
            expected_post: ManifestId,
            target_pre: ManifestId,
            phase: TransactionPhase,
        ) -> TransactionId {
            let expected = self.owner.load_manifest(&expected_post).unwrap();
            let target = self.owner.load_manifest(&target_pre).unwrap();
            let changes = plan_changes(&expected, &target);
            let id = TransactionId::random().unwrap();
            let recovery_path = self.owner.recovery_root().join(id.encoded());
            private_dir_all(&recovery_path).unwrap();
            let marker = TransactionMarker {
                version: TRANSACTION_VERSION,
                id,
                expected_post,
                observed_post_manifest: None,
                target_pre,
                candidate_session_id: Some("crash-fixture".into()),
                restored_manifest: None,
                rollback_manifest: None,
                recovery_path,
                temporary_paths: vec![],
                phase,
            };
            self.owner
                .prepare_recovery(&expected, &target, &changes, &marker)
                .unwrap();
            self.owner.write_marker(&marker).unwrap();
            id
        }
    }

    #[test]
    fn read_only_open_distinguishes_absence_and_never_repairs_existing_store() {
        let fixture = Fixture::new();
        let missing = fixture.store.parent().unwrap().join("missing-rewind-store");
        assert!(
            WorkspaceOwner::open_existing_read_only(
                &fixture.project,
                &missing,
                fixture.owner.limits,
            )
            .unwrap()
            .is_none()
        );
        assert!(!missing.exists());

        let index = fixture.store.join("blob-index.json");
        fs::write(&index, b"{\"stale\":1}").unwrap();
        let before = store_snapshot(&fixture.store);
        let owner = WorkspaceOwner::open_existing_read_only(
            &fixture.project,
            &fixture.store,
            fixture.owner.limits,
        )
        .unwrap()
        .unwrap();
        assert_eq!(owner.project_root(), fixture.project.as_path());
        assert_eq!(store_snapshot(&fixture.store), before);

        let namespace = fixture.store.join("namespace.json");
        fs::remove_file(&namespace).unwrap();
        assert!(
            WorkspaceOwner::open_existing_read_only(
                &fixture.project,
                &fixture.store,
                fixture.owner.limits,
            )
            .is_err()
        );
        assert!(!namespace.exists());
    }

    #[test]
    fn shared_activity_lease_blocks_the_existing_rewind_writer() {
        let fixture = Fixture::new();
        let activity_owner =
            crate::project_mutation::ProjectMutationOwnerV1::at_store_root(&fixture.store).unwrap();
        let activity = activity_owner.try_activity().unwrap();
        assert!(matches!(
            fixture.owner.capture(&fixture.policy()),
            Err(WorkspaceError {
                kind: ErrorKind::Busy,
                ..
            })
        ));
        drop(activity);
        assert!(fixture.owner.capture(&fixture.policy()).is_ok());
    }

    #[test]
    fn only_first_unbound_legacy_resume_can_create_workspace_identity() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("source.txt"), "original").unwrap();
        assert!(!fixture.owner.has_workspace_identity().unwrap());
        let first = fixture
            .owner
            .bind_first_legacy_head(&fixture.policy(), "first-session", |_, _| Ok(()))
            .unwrap();
        assert!(fixture.owner.has_workspace_identity().unwrap());
        assert!(fixture.owner.matches_manifest(&first).unwrap());
        let second = fixture
            .owner
            .bind_first_legacy_head(&fixture.policy(), "second-session", |_, _| Ok(()))
            .unwrap_err();
        assert_eq!(second.kind, ErrorKind::Conflict);
    }

    #[test]
    fn failed_first_binding_can_retry_only_for_its_reserved_session() {
        let fixture = Fixture::new();
        let file = fixture.project.join("source.txt");
        fs::write(&file, "original").unwrap();
        let failed =
            fixture
                .owner
                .bind_first_legacy_head(&fixture.policy(), "first-session", |_, _| {
                    fs::write(&file, "external edit")?;
                    Ok(())
                });
        assert_eq!(failed.unwrap_err().kind, ErrorKind::Conflict);
        assert!(
            fixture
                .owner
                .pending_legacy_binding_for("first-session")
                .unwrap()
        );
        assert_eq!(
            fixture
                .owner
                .bind_first_legacy_head(&fixture.policy(), "other-session", |_, _| Ok(()))
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        let manifest = fixture
            .owner
            .bind_first_legacy_head(&fixture.policy(), "first-session", |_, _| Ok(()))
            .unwrap();
        assert!(fixture.owner.matches_manifest(&manifest).unwrap());
        assert!(
            !fixture
                .owner
                .pending_legacy_binding_for("first-session")
                .unwrap()
        );
    }

    #[test]
    fn sidecar_write_failure_retains_a_retryable_first_binding_reservation() {
        let fixture = Fixture::new();
        let failed =
            fixture
                .owner
                .bind_first_legacy_head(&fixture.policy(), "first-session", |_, _| {
                    Err(std::io::Error::other("sidecar write failed"))
                });
        assert_eq!(failed.unwrap_err().kind, ErrorKind::Io);
        assert!(
            fixture
                .owner
                .pending_legacy_binding_for("first-session")
                .unwrap()
        );
        fixture
            .owner
            .bind_first_legacy_head(&fixture.policy(), "first-session", |_, _| Ok(()))
            .unwrap();
    }

    #[test]
    fn opaque_ids_require_lower_hex() {
        let id = ManifestId::random().unwrap();
        assert_eq!(id.to_string().parse::<ManifestId>().unwrap(), id);
        assert!(id.to_string().to_uppercase().parse::<ManifestId>().is_err());
        assert!("00".parse::<ManifestId>().is_err());
    }

    #[test]
    fn apply_does_not_replace_a_file_created_after_the_absence_check() {
        let fixture = Fixture::new();
        let destination = fixture.project.join("raced.txt");
        fs::write(&destination, b"checkpoint content").unwrap();
        let target = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::remove_file(&destination).unwrap();
        let expected = fixture.owner.capture(&fixture.policy()).unwrap();

        INSTALL_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| {
                fs::write(path, b"external editor content").unwrap();
            }));
        });
        let result = fixture.owner.restore(&expected, &target);
        assert!(matches!(
            result,
            Err(WorkspaceError {
                kind: ErrorKind::Conflict,
                ..
            })
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"external editor content");
        assert!(fixture.owner.resolve_pending().is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"external editor content");
    }

    #[test]
    fn apply_does_not_delete_a_file_replaced_after_the_identity_check() {
        let fixture = Fixture::new();
        let destination = fixture.project.join("raced.txt");
        let target = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(&destination, b"checkpoint content").unwrap();
        let expected = fixture.owner.capture(&fixture.policy()).unwrap();

        REMOVE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| {
                fs::remove_file(path).unwrap();
                fs::write(path, b"external editor content").unwrap();
            }));
        });
        let result = fixture.owner.restore(&expected, &target);
        assert!(matches!(
            result,
            Err(WorkspaceError {
                kind: ErrorKind::Conflict,
                ..
            })
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"external editor content");
        assert!(fixture.owner.resolve_pending().is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"external editor content");
    }

    #[test]
    fn resolver_refuses_an_external_deletion_without_a_quarantined_original() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let transaction = fixture.transaction_marker(post, pre, TransactionPhase::Applying);
        fs::remove_file(fixture.project.join("file.txt")).unwrap();

        assert_eq!(
            fixture.owner.resolve_pending().unwrap_err().kind,
            ErrorKind::Conflict
        );
        assert!(!fixture.project.join("file.txt").exists());
        assert_eq!(
            fixture.owner.load_marker(&transaction).unwrap().phase,
            TransactionPhase::Applying
        );
    }

    #[test]
    fn resolver_accepts_a_recorded_partial_removal_after_crash() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let transaction = fixture.transaction_marker(post, pre, TransactionPhase::Applying);
        let marker = fixture.owner.load_marker(&transaction).unwrap();
        let held = marker
            .recovery_path
            .join("removed-forward")
            .join(recovery_entry_name(Path::new("file.txt")));
        private_dir_all(held.parent().unwrap()).unwrap();
        rename_noreplace(&fixture.project.join("file.txt"), &held).unwrap();
        sync_parent(&held).unwrap();

        assert!(matches!(
            fixture.owner.resolve_pending().unwrap(),
            ResolveOutcome::RolledBack(_)
        ));
        assert_eq!(fs::read(fixture.project.join("file.txt")).unwrap(), b"post");
    }

    #[test]
    fn resolver_recovers_after_a_killed_process_at_each_forward_phase() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::Command;

        for phase in ["prepared", "applying", "pending-activation"] {
            let fixture = Fixture::new();
            fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
            let pre = fixture.owner.capture(&fixture.policy()).unwrap();
            fs::write(fixture.project.join("file.txt"), b"post").unwrap();
            fs::create_dir(fixture.project.join("later-dir")).unwrap();
            let post = fixture.owner.capture(&fixture.policy()).unwrap();
            let transaction = if phase == "pending-activation" {
                None
            } else {
                Some(fixture.transaction_marker(post, pre, TransactionPhase::Prepared))
            };

            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--ignored")
                .arg("--exact")
                .arg("rewind_workspace::tests::killed_process_rewind_worker")
                .arg("--test-threads=1")
                .env("POLARIS_REWIND_CRASH_PHASE", phase)
                .env("POLARIS_REWIND_CRASH_PROJECT", &fixture.project)
                .env("POLARIS_REWIND_CRASH_STORE", &fixture.store)
                .env("POLARIS_REWIND_CRASH_PRE", pre.to_string())
                .env("POLARIS_REWIND_CRASH_POST", post.to_string())
                .env(
                    "POLARIS_REWIND_CRASH_TRANSACTION",
                    transaction.map_or_else(String::new, |id| id.to_string()),
                )
                .status()
                .unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL), "phase={phase}");

            let reopened =
                WorkspaceOwner::open(&fixture.project, &fixture.store, fixture.owner.limits)
                    .unwrap();
            assert!(matches!(
                reopened.resolve_pending().unwrap(),
                ResolveOutcome::RolledBack(ref outcomes) if outcomes.len() == 1
            ));
            assert_eq!(fs::read(fixture.project.join("file.txt")).unwrap(), b"post");
            assert!(fixture.project.join("later-dir").is_dir());
        }
    }

    #[test]
    #[ignore = "subprocess worker for the actual SIGKILL recovery test"]
    fn killed_process_rewind_worker() {
        let Ok(phase) = std::env::var("POLARIS_REWIND_CRASH_PHASE") else {
            return;
        };
        let project = PathBuf::from(std::env::var("POLARIS_REWIND_CRASH_PROJECT").unwrap());
        let store = PathBuf::from(std::env::var("POLARIS_REWIND_CRASH_STORE").unwrap());
        let pre: ManifestId = std::env::var("POLARIS_REWIND_CRASH_PRE")
            .unwrap()
            .parse()
            .unwrap();
        let post: ManifestId = std::env::var("POLARIS_REWIND_CRASH_POST")
            .unwrap()
            .parse()
            .unwrap();
        let owner = WorkspaceOwner::open(&project, &store, RewindLimits::default()).unwrap();
        match phase.as_str() {
            "prepared" => {}
            "applying" => {
                let transaction: TransactionId = std::env::var("POLARIS_REWIND_CRASH_TRANSACTION")
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut marker = owner.load_marker(&transaction).unwrap();
                marker.phase = TransactionPhase::Applying;
                owner.write_marker(&marker).unwrap();
                let held = marker
                    .recovery_path
                    .join("removed-forward")
                    .join(recovery_entry_name(Path::new("file.txt")));
                private_dir_all(held.parent().unwrap()).unwrap();
                rename_noreplace(&project.join("file.txt"), &held).unwrap();
                sync_parent(&held).unwrap();
            }
            "pending-activation" => {
                owner.restore(&post, &pre).unwrap();
            }
            _ => panic!("unknown phase"),
        }
        unsafe { libc::raise(libc::SIGKILL) };
        unreachable!();
    }

    #[test]
    fn directory_remove_preserves_an_external_child_created_during_apply() {
        let fixture = Fixture::new();
        let directory = fixture.project.join("dir");
        let target = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("old.txt"), b"owned").unwrap();
        let expected = fixture.owner.capture(&fixture.policy()).unwrap();

        DIRECTORY_REMOVE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| {
                fs::write(path.join("external.txt"), b"external editor content").unwrap();
            }));
        });
        assert_eq!(
            fixture.owner.restore(&expected, &target).unwrap_err().kind,
            ErrorKind::Conflict
        );
        assert_eq!(
            fs::read(directory.join("external.txt")).unwrap(),
            b"external editor content"
        );
        assert!(fixture.owner.resolve_pending().is_err());
        assert_eq!(
            fs::read(directory.join("external.txt")).unwrap(),
            b"external editor content"
        );
    }

    #[test]
    fn commit_refuses_a_late_write_to_a_quarantined_directory() {
        let fixture = Fixture::new();
        let target = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::create_dir(fixture.project.join("dir")).unwrap();
        let expected = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture.owner.restore(&expected, &target).unwrap();
        let held = outcome
            .recovery_path
            .join("removed-forward")
            .join(recovery_entry_name(Path::new("dir")));
        fs::write(held.join("external.txt"), b"late editor write").unwrap();

        assert_eq!(
            fixture
                .owner
                .commit(&outcome.transaction_id)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(
            fs::read(held.join("external.txt")).unwrap(),
            b"late editor write"
        );
        assert_eq!(
            fixture
                .owner
                .load_marker(&outcome.transaction_id)
                .unwrap()
                .phase,
            TransactionPhase::PendingActivation
        );
    }

    #[test]
    fn install_refuses_a_parent_replaced_by_an_external_symlink() {
        let fixture = Fixture::new();
        let parent = fixture.project.join("sub");
        let external = fixture.store.join("external");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&external).unwrap();
        fs::write(parent.join("new.txt"), b"checkpoint content").unwrap();
        let target = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::remove_file(parent.join("new.txt")).unwrap();
        let expected = fixture.owner.capture(&fixture.policy()).unwrap();

        INSTALL_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |destination| {
                let parent = destination.parent().unwrap();
                fs::rename(parent, parent.with_file_name("sub-hidden")).unwrap();
                std::os::unix::fs::symlink(&external, parent).unwrap();
            }));
        });
        assert!(fixture.owner.restore(&expected, &target).is_err());
        assert!(!fixture.store.join("external/new.txt").exists());
        assert!(fixture.project.join("sub-hidden").is_dir());
    }

    #[test]
    fn capture_preview_restore_and_commit_cover_files_and_directories() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.project.join("nested")).unwrap();
        fs::set_permissions(
            fixture.project.join("nested"),
            Permissions::from_mode(0o750),
        )
        .unwrap();
        fs::write(fixture.project.join("nested/kept.txt"), b"before").unwrap();
        fs::write(fixture.project.join("deleted.txt"), b"restore-me").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();

        fs::write(fixture.project.join("nested/kept.txt"), b"after").unwrap();
        fs::set_permissions(
            fixture.project.join("nested"),
            Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::remove_file(fixture.project.join("deleted.txt")).unwrap();
        fs::create_dir(fixture.project.join("created")).unwrap();
        fs::write(fixture.project.join("created/new.txt"), b"new").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();

        let preview = fixture.owner.preview(&post, &pre).unwrap();
        assert!(preview.restorable);
        assert_eq!(preview.changes.len(), 5);
        let outcome = fixture.owner.restore(&post, &pre).unwrap();
        assert_eq!(
            fs::read(fixture.project.join("nested/kept.txt")).unwrap(),
            b"before"
        );
        assert_eq!(
            fs::read(fixture.project.join("deleted.txt")).unwrap(),
            b"restore-me"
        );
        assert_eq!(
            fs::metadata(fixture.project.join("nested")).unwrap().mode() & 0o777,
            0o750
        );
        assert!(!fixture.project.join("created").exists());
        assert!(outcome.recovery_path.starts_with(&fixture.store));
        fixture.owner.commit(&outcome.transaction_id).unwrap();
        assert!(fixture.owner.has_committed_rewind().unwrap());
        assert!(
            fixture
                .owner
                .matches_manifest(&outcome.restored_manifest_id)
                .unwrap()
        );
        assert!(fixture.owner.matches_manifest(&pre).unwrap());
    }

    #[test]
    fn restore_rejects_external_change_after_expected_post_capture() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"external").unwrap();
        let error = fixture.owner.restore(&post, &pre).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict);
        assert_eq!(
            fs::read(fixture.project.join("file.txt")).unwrap(),
            b"external"
        );
    }

    #[test]
    fn pending_activation_rolls_back_on_startup_resolution() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture.owner.restore(&post, &pre).unwrap();
        assert_eq!(fs::read(fixture.project.join("file.txt")).unwrap(), b"pre");
        let resolved = fixture.owner.resolve_pending().unwrap();
        assert!(matches!(resolved, ResolveOutcome::RolledBack(ref values) if values.len() == 1));
        assert_eq!(fs::read(fixture.project.join("file.txt")).unwrap(), b"post");
        assert!(outcome.recovery_path.exists());
    }

    #[test]
    fn branch_restore_persists_candidate_before_activation() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-session-1234")
            .unwrap();
        let marker = fixture.owner.load_marker(&outcome.transaction_id).unwrap();
        assert_eq!(
            marker.candidate_session_id.as_deref(),
            Some("candidate-session-1234")
        );
        assert_eq!(marker.phase, TransactionPhase::PendingActivation);
    }

    #[test]
    fn rolled_back_branch_allows_a_new_attempt_at_the_same_boundary() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let first = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-first-1234")
            .unwrap();
        fixture.owner.rollback(&first.transaction_id).unwrap();

        let retry = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-retry-1234")
            .unwrap();
        assert_eq!(
            fixture
                .owner
                .load_marker(&retry.transaction_id)
                .unwrap()
                .phase,
            TransactionPhase::PendingActivation
        );
        assert_eq!(fs::read(fixture.project.join("file.txt")).unwrap(), b"pre");
        assert!(matches!(
            fixture.owner.resolve_pending().unwrap(),
            ResolveOutcome::RolledBack(ref outcomes) if outcomes.len() == 1
        ));
        assert_eq!(fs::read(fixture.project.join("file.txt")).unwrap(), b"post");
    }

    #[test]
    fn rolled_back_branch_rejects_external_same_value_replacement() {
        let fixture = Fixture::new();
        let file = fixture.project.join("file.txt");
        fs::write(&file, b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(&file, b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let first = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-first-1234")
            .unwrap();
        fixture.owner.rollback(&first.transaction_id).unwrap();
        let replacement = fixture.project.join("replacement.txt");
        fs::write(&replacement, b"post").unwrap();
        fs::rename(&replacement, &file).unwrap();

        assert_eq!(
            fixture
                .owner
                .restore_for_branch(&post, &pre, "candidate-retry-1234")
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(fs::read(&file).unwrap(), b"post");
    }

    #[test]
    fn pending_branch_denies_execution_activity_until_resolved() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-session-1234")
            .unwrap();

        assert!(fixture.owner.try_activity_quiescent().is_err());
        assert!(matches!(
            fixture.owner.resolve_pending().unwrap(),
            ResolveOutcome::RolledBack(_)
        ));
        assert!(fixture.owner.try_activity_quiescent().is_ok());
    }

    #[test]
    fn branch_transaction_evidence_requires_a_unique_committed_match() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-session-1234")
            .unwrap();

        assert!(
            fixture
                .owner
                .has_branch_transaction_candidate("candidate-session-1234")
                .unwrap()
        );
        assert_eq!(
            fixture.owner.branch_transaction_candidate_ids().unwrap(),
            vec!["candidate-session-1234"]
        );
        assert!(matches!(
            fixture
                .owner
                .committed_branch_transaction("candidate-session-1234", &post, &pre,),
            Err(WorkspaceError {
                kind: ErrorKind::InvalidTransaction,
                ..
            })
        ));

        fixture.owner.commit(&outcome.transaction_id).unwrap();
        let evidence = fixture
            .owner
            .committed_branch_transaction("candidate-session-1234", &post, &pre)
            .unwrap();
        assert_eq!(evidence.transaction_id, outcome.transaction_id);
        assert_eq!(evidence.restored_manifest_id, outcome.restored_manifest_id);
    }

    #[test]
    fn branch_transaction_evidence_rejects_manifest_mismatch() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-session-1234")
            .unwrap();
        fixture.owner.commit(&outcome.transaction_id).unwrap();

        assert!(matches!(
            fixture
                .owner
                .committed_branch_transaction("candidate-session-1234", &pre, &pre,),
            Err(WorkspaceError {
                kind: ErrorKind::InvalidTransaction,
                ..
            })
        ));
    }

    #[test]
    fn branch_transaction_evidence_rejects_duplicate_candidate_markers() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture
            .owner
            .restore_for_branch(&post, &pre, "candidate-session-1234")
            .unwrap();
        fixture.owner.commit(&outcome.transaction_id).unwrap();
        let mut duplicate = fixture.owner.load_marker(&outcome.transaction_id).unwrap();
        duplicate.id = TransactionId::random().unwrap();
        duplicate.recovery_path = fixture.owner.recovery_root().join(duplicate.id.encoded());
        private_dir_all(&duplicate.recovery_path).unwrap();
        fixture.owner.write_marker(&duplicate).unwrap();

        assert!(
            fixture
                .owner
                .has_branch_transaction_candidate("candidate-session-1234")
                .is_err()
        );
        assert!(fixture.owner.branch_transaction_candidate_ids().is_err());
        assert!(
            fixture
                .owner
                .committed_branch_transaction("candidate-session-1234", &post, &pre)
                .is_err()
        );
    }

    #[test]
    fn resolver_handles_prepared_and_partial_applying_markers() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("a.txt"), b"pre-a").unwrap();
        fs::write(fixture.project.join("b.txt"), b"pre-b").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("a.txt"), b"post-a").unwrap();
        fs::write(fixture.project.join("b.txt"), b"post-b").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();

        let prepared = fixture.transaction_marker(post, pre, TransactionPhase::Prepared);
        let result = fixture.owner.resolve_pending().unwrap();
        assert!(matches!(result, ResolveOutcome::RolledBack(ref values) if values.len() == 1));
        assert_eq!(
            fixture.owner.load_marker(&prepared).unwrap().phase,
            TransactionPhase::RolledBack
        );

        let applying = fixture.transaction_marker(post, pre, TransactionPhase::Applying);
        fs::write(fixture.project.join("a.txt"), b"pre-a").unwrap();
        let result = fixture.owner.resolve_pending().unwrap();
        assert!(matches!(result, ResolveOutcome::RolledBack(ref values) if values.len() == 1));
        assert_eq!(fs::read(fixture.project.join("a.txt")).unwrap(), b"post-a");
        assert_eq!(fs::read(fixture.project.join("b.txt")).unwrap(), b"post-b");
        assert_eq!(
            fixture.owner.load_marker(&applying).unwrap().phase,
            TransactionPhase::RolledBack
        );
    }

    #[test]
    fn resolver_does_not_overwrite_an_external_edit_after_restore() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.owner.capture(&fixture.policy()).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.owner.capture(&fixture.policy()).unwrap();
        let outcome = fixture.owner.restore(&post, &pre).unwrap();
        fs::write(fixture.project.join("file.txt"), b"external").unwrap();
        let error = fixture.owner.resolve_pending().unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict);
        assert_eq!(
            fs::read(fixture.project.join("file.txt")).unwrap(),
            b"external"
        );
        assert_eq!(
            fixture
                .owner
                .load_marker(&outcome.transaction_id)
                .unwrap()
                .phase,
            TransactionPhase::PendingActivation
        );
    }

    #[test]
    #[ignore = "local capture timing evidence; no stable latency threshold"]
    fn measure_small_and_large_capture_time_and_retained_bytes() {
        for (label, count) in [("small", 32usize), ("large", 2_000usize)] {
            let mut timings = Vec::new();
            let mut retained = Vec::new();
            let mut failures: BTreeMap<String, usize> = BTreeMap::new();
            for repetition in 0..5 {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path().canonicalize().unwrap();
                let project = root.join("project");
                let store = root.join("store");
                fs::create_dir(&project).unwrap();
                let owner =
                    WorkspaceOwner::open(&project, &store, RewindLimits::default()).unwrap();
                let policy =
                    SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project))
                        .unwrap();
                for index in 0..count {
                    fs::write(
                        project.join(format!("file-{index:04}.txt")),
                        format!("fixture-{label}-{repetition}-{index}"),
                    )
                    .unwrap();
                }
                let started = Instant::now();
                let result = owner.capture(&policy);
                let elapsed = started.elapsed();
                let bytes = owner.blob_bytes().unwrap();
                timings.push(elapsed);
                retained.push(bytes);
                if let Err(error) = &result {
                    *failures.entry(format!("{:?}", error.kind)).or_default() += 1;
                }
                eprintln!(
                    "rewind-capture label={label} repetition={repetition} files={count} elapsed_us={} retained_bytes={bytes} result={:?}",
                    elapsed.as_micros(),
                    result.as_ref().map(|_| ()).map_err(|error| error.kind)
                );
            }
            timings.sort();
            retained.sort();
            eprintln!(
                "rewind-capture-summary label={label} samples=5 p50_us={} p95_us={} retained_p50_bytes={} retained_p95_bytes={} failures={failures:?}",
                timings[2].as_micros(),
                timings[4].as_micros(),
                retained[2],
                retained[4],
            );
        }
    }

    #[test]
    fn path_eligibility_rejects_outside_git_cache_symlink_and_hardlink() {
        let fixture = Fixture::new();
        fs::write(fixture.project.join("ordinary"), b"ok").unwrap();
        fs::create_dir(fixture.project.join("target")).unwrap();
        std::os::unix::fs::symlink("ordinary", fixture.project.join("alias")).unwrap();
        fs::hard_link(
            fixture.project.join("ordinary"),
            fixture.project.join("hard"),
        )
        .unwrap();
        assert!(
            !fixture
                .owner
                .is_restorable_path(Path::new("../outside"))
                .unwrap()
        );
        assert!(
            !fixture
                .owner
                .is_restorable_path(Path::new(".git/config"))
                .unwrap()
        );
        assert!(
            !fixture
                .owner
                .is_restorable_path(Path::new("target/out"))
                .unwrap()
        );
        assert!(
            !fixture
                .owner
                .is_restorable_path(Path::new("alias"))
                .unwrap()
        );
        assert!(!fixture.owner.is_restorable_path(Path::new("hard")).unwrap());
    }
}
