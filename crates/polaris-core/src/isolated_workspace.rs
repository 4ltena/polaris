//! Bounded, sanitized source snapshots. No apply-back or execution policy lives here.
//! Callers must serialize credential registration/rotation with snapshot creation.
//! This is not protection against a malicious, unsandboxed process of the same UID.

#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::fs::{self, File, Permissions};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Instant;
use tempfile::TempDir;

#[cfg(unix)]
pub use polaris_tools::path_policy::tree_admission::RegisteredDirectory;
pub use polaris_tools::path_policy::tree_admission::{
    ErrorKind, Exclusion, ExclusionReason, Identity, Limits, RegisteredSecrets, SnapshotError,
};
#[cfg(unix)]
use polaris_tools::path_policy::tree_admission::{
    Selection, io, open_root_with_ancestors, unchanged,
};
use polaris_tools::path_policy::tree_admission::{error, is_repository_metadata};

#[derive(Clone, Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct ManifestEntry {
    pub relative_path: PathBuf,
    pub identity: Identity,
    pub bytes: u64,
    /// Only ordinary rwx bits; setid and sticky bits are never preserved.
    pub mode: u32,
    /// None for directories.
    pub sha256: Option<[u8; 32]>,
    // Preserve special permission bits for source conflict detection only.
    // They must never be applied to the output workspace.
    observed_mode: u32,
    observed_mtime: (i64, i64),
    observed_ctime: (i64, i64),
}

impl ManifestEntry {
    /// Original source permission bits for approval/conflict evidence only.
    /// Use `mode` for installing output; special bits must not be restored.
    pub fn observed_mode(&self) -> u32 {
        self.observed_mode
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn observed_mtime(&self) -> (i64, i64) {
        self.observed_mtime
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn observed_ctime(&self) -> (i64, i64) {
        self.observed_ctime
    }
}

type Result<T> = std::result::Result<T, SnapshotError>;

/// Owns a private temporary root containing workspace, home and tmp siblings.
/// Dropping this value removes all three, including scratch contents.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Snapshot {
    _root: TempDir,
    workspace: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    copy_identity: Identity,
    source_ancestors: Vec<(PathBuf, Identity)>,
    pub source_path: PathBuf,
    pub source_identity: Identity,
    pub manifest: Vec<ManifestEntry>,
    pub exclusions: Vec<Exclusion>,
    exclude_build_caches: bool,
}
impl Snapshot {
    /// Sanitized workspace; only this directory participates in change scans.
    pub fn path(&self) -> &Path {
        &self.workspace
    }

    /// Private HOME sibling, excluded structurally from workspace change scans.
    pub fn scratch_home(&self) -> &Path {
        &self.home
    }

    /// Private temporary-file sibling, with the same lifetime as the workspace.
    pub fn scratch_tmp(&self) -> &Path {
        &self.tmp
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone)]
pub struct D27SourceArtifactsV1 {
    pub binding: crate::apply_intent::D27SourceBindingV1,
    pub request: crate::apply_intent::RequestV1,
    pub source_before: crate::apply_intent::ObservedSourceV1,
}

/// Projects only approved changed files. It reads both held ownership
/// coordinates again; no artifact is published and no apply is authorized.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn project_d27_source_m2a(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    record: &crate::desktop_store::SavedSourceApply,
    recovery: &crate::workspace_apply::RecoveryParent,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<D27SourceArtifactsV1> {
    use crate::apply_intent::{
        BeforeV1, CanonicalApplyEvidence, D27ManifestProjectionEntryV1, D27ManifestProjectionV1,
        D27ManifestVersionV1, D27SourceBindingV1, IdentityV1, ObservedEntryV1, ObservedSourceV1,
    };
    use crate::management_effect::ManagementEffectHashV1 as Hash;
    use polaris_desktop_protocol::snapshot::ApprovalDecision;
    let fail = || error(&snapshot.source_path, ErrorKind::SourceChanged);
    if record.decision != Some(ApprovalDecision::Allow)
        || record.invalidated
        || record.intent_revision.is_some()
        || record.reserved_result_id.is_some()
        || record.result.is_some()
        || !changes.is_applicable()
        || changes.snapshot_path != snapshot.path()
        || changes.copy_identity != snapshot.copy_identity
        || changes.source_identity != snapshot.source_identity
    {
        return Err(fail());
    }
    validate_baseline(snapshot, limits)?;
    let candidate = &record.candidate;
    candidate.payload.validate().map_err(|_| fail())?;
    if candidate.payload.payload_hash().map_err(|_| fail())? != candidate.payload_hash {
        return Err(fail());
    }
    let payload = &candidate.payload;
    let (source_fd, source_ancestors) = open_root_with_ancestors(&snapshot.source_path)?;
    if source_ancestors != snapshot.source_ancestors
        || Identity::of(&io(&snapshot.source_path, source_fd.metadata())?)
            != snapshot.source_identity
    {
        return Err(fail());
    }
    recovery
        .validate_for_snapshot(snapshot)
        .map_err(|_| fail())?;
    let (recovery_device, recovery_inode) = recovery.identity();
    if snapshot.source_path.to_str() != Some(payload.source_path.as_str())
        || payload.source_identity.device.get() != snapshot.source_identity.device
        || payload.source_identity.inode.get() != snapshot.source_identity.inode
        || recovery.provenance().to_str() != Some(payload.recovery_parent_path.as_str())
        || payload.recovery_parent_identity.device.get() != recovery_device
        || payload.recovery_parent_identity.inode.get() != recovery_inode
        || payload.entries.len() != changes.changes.len()
    {
        return Err(fail());
    }
    let same_entry = |a: &Option<ManifestEntry>, b: &Option<ManifestEntry>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.relative_path == b.relative_path
                && a.identity == b.identity
                && a.bytes == b.bytes
                && a.mode == b.mode
                && a.sha256 == b.sha256
                && a.observed_mode == b.observed_mode
                && a.observed_mtime == b.observed_mtime
                && a.observed_ctime == b.observed_ctime
        }
        _ => false,
    };
    let check_changes = || -> Result<()> {
        let fresh = collect_changes(snapshot, secrets, limits)?;
        if !fresh.is_applicable() || fresh.changes.len() != changes.changes.len() {
            return Err(fail());
        }
        let indexed = changes
            .changes
            .iter()
            .map(|change| (change.relative_path.as_path(), change))
            .collect::<BTreeMap<_, _>>();
        if indexed.len() != changes.changes.len() {
            return Err(fail());
        }
        for change in &fresh.changes {
            let supplied = indexed
                .get(change.relative_path.as_path())
                .ok_or_else(fail)?;
            if supplied.kind != change.kind
                || !same_entry(&supplied.before, &change.before)
                || !same_entry(&supplied.after, &change.after)
            {
                return Err(fail());
            }
        }
        Ok(())
    };
    check_changes()?;
    let version = |entry: &ManifestEntry| -> Result<D27ManifestVersionV1> {
        let bytes = entry.sha256.ok_or_else(fail)?;
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(D27ManifestVersionV1 {
            device: entry.identity.device,
            inode: entry.identity.inode,
            hash: Hash::from_hex(&hex).map_err(|_| fail())?,
            normal_mode: entry.mode,
            observed_mode: entry.observed_mode(),
            bytes: entry.bytes,
        })
    };
    let indexed = changes
        .changes
        .iter()
        .map(|change| (change.relative_path.as_path(), change))
        .collect::<BTreeMap<_, _>>();
    let mut entries = Vec::with_capacity(payload.entries.len());
    let mut selection = Selection::default();
    for entry in &payload.entries {
        let path = Path::new(&entry.relative_path);
        let change = indexed.get(path).ok_or_else(fail)?;
        let before = change.before.as_ref().map(&version).transpose()?;
        let after = change.after.as_ref().map(&version).transpose()?;
        let matches = |wire: &Option<crate::desktop_store::SourceApplyVersion>,
                       manifest: &Option<D27ManifestVersionV1>,
                       observed: bool| {
            match (wire, manifest) {
                (None, None) => true,
                (Some(w), Some(m)) => {
                    w.hash == m.hash.as_hex()
                        && w.mode
                            == if observed {
                                m.observed_mode
                            } else {
                                m.normal_mode
                            }
                }
                _ => false,
            }
        };
        if !matches(&entry.before, &before, true) || !matches(&entry.after, &after, false) {
            return Err(fail());
        }
        for ancestor in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            selection.exact.insert(ancestor.to_path_buf());
        }
        entries.push(D27ManifestProjectionEntryV1 {
            path: entry.relative_path.clone(),
            before,
            after,
        });
    }
    let manifest = D27ManifestProjectionV1 {
        schema_version: 1,
        source_path: payload.source_path.clone(),
        source_identity: IdentityV1 {
            device: snapshot.source_identity.device,
            inode: snapshot.source_identity.inode,
        },
        recovery_parent_path: payload.recovery_parent_path.clone(),
        recovery_parent_identity: IdentityV1 {
            device: recovery_device,
            inode: recovery_inode,
        },
        entries,
    };
    let request = manifest.project_request().map_err(|_| fail())?;
    let observe = || -> Result<ObservedSourceV1> {
        let current = scan(
            &snapshot.source_path,
            &snapshot.source_path,
            snapshot.source_identity,
            &refreshed_secrets(secrets)?,
            limits,
            Some(&selection),
            snapshot.exclude_build_caches,
        )?;
        if !current.exclusions.is_empty() {
            return Err(fail());
        }
        let indexed = current
            .manifest
            .iter()
            .map(|entry| (entry.relative_path.as_path(), entry))
            .collect::<BTreeMap<_, _>>();
        let mut entries = Vec::with_capacity(request.entries.len());
        for entry in &request.entries {
            let current = indexed
                .get(Path::new(&entry.path))
                .map(|entry| {
                    let v = version(entry)?;
                    Ok::<BeforeV1, SnapshotError>(BeforeV1 {
                        device: v.device,
                        inode: v.inode,
                        hash: v.hash,
                        mode: v.normal_mode,
                        observed_mode: v.observed_mode,
                        bytes: v.bytes,
                    })
                })
                .transpose()?;
            entries.push(ObservedEntryV1 {
                path: entry.path.clone(),
                current,
            });
        }
        Ok(ObservedSourceV1 {
            schema_version: 1,
            source_identity: request.source_identity,
            entries,
        })
    };
    let source_before = observe()?;
    source_before.verify_before(&request).map_err(|_| fail())?;
    let payload_bytes = serde_json::to_vec(payload).map_err(|_| fail())?;
    let manifest_bytes = manifest.canonical_bytes().map_err(|_| fail())?;
    let request_bytes = request.canonical_bytes().map_err(|_| fail())?;
    let before_bytes = source_before.canonical_bytes().map_err(|_| fail())?;
    let binding = D27SourceBindingV1 {
        schema_version: 1,
        payload_bytes: u64::try_from(payload_bytes.len()).map_err(|_| fail())?,
        payload_hash: Hash::digest(&payload_bytes),
        manifest_bytes: u64::try_from(manifest_bytes.len()).map_err(|_| fail())?,
        manifest_hash: manifest.manifest_hash().map_err(|_| fail())?,
        request_bytes: u64::try_from(request_bytes.len()).map_err(|_| fail())?,
        request_hash: request.request_hash().map_err(|_| fail())?,
        source_before_bytes: u64::try_from(before_bytes.len()).map_err(|_| fail())?,
        source_before_hash: source_before.observation_hash().map_err(|_| fail())?,
        manifest,
    };
    binding.canonical_bytes().map_err(|_| fail())?;
    check_changes()?;
    if observe()? != source_before
        || open_root_with_ancestors(&snapshot.source_path)?.1 != source_ancestors
        || Identity::of(&io(&snapshot.source_path, source_fd.metadata())?)
            != snapshot.source_identity
    {
        return Err(fail());
    }
    recovery
        .validate_for_snapshot(snapshot)
        .map_err(|_| fail())?;
    Ok(D27SourceArtifactsV1 {
        binding,
        request,
        source_before,
    })
}

/// The supplied source must be an absolute physical path, with no symlink
/// ancestors. Output is created under the OS temp directory, outside source.
/// Bounds or detected races fail the entire operation and discard partial output.
#[cfg(unix)]
pub fn snapshot(source: &Path, secrets: &RegisteredSecrets, limits: Limits) -> Result<Snapshot> {
    snapshot_inner(source, secrets, limits, None, false, || {})
}

/// Authentication locations must be registered by their loader before this call.
/// Copying stays under the auth lock; credentials themselves never enter the copy.
#[cfg(unix)]
pub fn protected_snapshot(source: &Path, limits: Limits) -> Result<Snapshot> {
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered)?;
        snapshot(source, &secrets, limits)
    })
    .map_err(|_| error(source, ErrorKind::ProtectionUnavailable))?
}

/// Rewind capture uses the same no-follow protected walk, but enforces the
/// admission deadline while omitting well-known ephemeral build directories.
#[cfg(unix)]
pub(crate) fn protected_rewind_snapshot(
    source: &Path,
    limits: Limits,
    deadline: Option<Instant>,
) -> Result<Snapshot> {
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered)?;
        rewind_snapshot(source, &secrets, limits, deadline)
    })
    .map_err(|_| error(source, ErrorKind::ProtectionUnavailable))?
}

#[cfg(unix)]
pub(crate) fn rewind_snapshot(
    source: &Path,
    secrets: &RegisteredSecrets,
    limits: Limits,
    deadline: Option<Instant>,
) -> Result<Snapshot> {
    snapshot_inner(source, secrets, limits, deadline, true, || {})
}

/// Capture candidate metadata under the authentication registry lock. Callers
/// must stop snapshot writers first and still revalidate at apply time. This
/// neither authorizes source writes nor keeps the lock after returning.
#[cfg(unix)]
pub fn protected_collect_changes(snapshot: &Snapshot, limits: Limits) -> Result<ChangeSet> {
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered)?;
        collect_changes(snapshot, &secrets, limits)
    })
    .map_err(|_| error(&snapshot.source_path, ErrorKind::ProtectionUnavailable))?
}

#[cfg(unix)]
fn snapshot_inner(
    source: &Path,
    secrets: &RegisteredSecrets,
    limits: Limits,
    deadline: Option<Instant>,
    exclude_build_caches: bool,
    after_open: impl FnOnce(),
) -> Result<Snapshot> {
    // Refresh identities for previously absent paths before any content read.
    // Retain old identities as well, covering a registered file moved aside.
    let refreshed = refreshed_secrets(secrets)?;
    let secrets = &refreshed;
    let (source_fd, source_ancestors) = open_root_with_ancestors(source)?;
    let initial = io(source, source_fd.metadata())?;
    if secrets.denies(source, &initial)
        || polaris_tools::path_policy::is_denied(source)
        || is_repository_metadata(source)
    {
        return Err(error(source, ErrorKind::InvalidPath));
    }
    let temp_parent = io(source, fs::canonicalize(std::env::temp_dir()))?;
    if temp_parent.starts_with(source) {
        return Err(error(source, ErrorKind::DestinationInsideSource));
    }
    let root = io(
        &temp_parent,
        tempfile::Builder::new()
            .prefix("polaris-workspace-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(&temp_parent),
    )?;
    // M6-5: recorded so a later start can remove it if this process is killed.
    crate::scratch_registry::record(
        root.path(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
    );
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let tmp = root.path().join("tmp");
    for directory in [&workspace, &home, &tmp] {
        io(
            directory,
            fs::DirBuilder::new().mode(0o700).create(directory),
        )?;
    }
    let copy_identity = Identity::of(&io(&workspace, fs::metadata(&workspace))?);
    let mut output = Snapshot {
        _root: root,
        workspace,
        home,
        tmp,
        copy_identity,
        source_ancestors: source_ancestors.clone(),
        source_path: source.into(),
        source_identity: Identity::of(&initial),
        manifest: Vec::new(),
        exclusions: Vec::new(),
        exclude_build_caches,
    };
    after_open();
    let mut copier = Copier {
        secrets,
        private_owner: None,
        limits,
        entries: 0,
        files: 0,
        bytes: 0,
        root_path: source,
        policy_root: source,
        destination: Some(output.path()),
        selection: None,
        manifest: Vec::new(),
        exclusions: Vec::new(),
        deadline,
        exclude_build_caches,
    };
    copier.walk(&source_fd, Path::new(""), 0)?;
    let manifest = copier.manifest;
    let exclusions = copier.exclusions;
    output.manifest = manifest;
    output.exclusions = exclusions;
    let (current_fd, current_ancestors) = open_root_with_ancestors(source)?;
    let current = io(source, current_fd.metadata())?;
    if !unchanged(&initial, &current) || source_ancestors != current_ancestors {
        return Err(error(source, ErrorKind::SourceChanged));
    }
    Ok(output)
}

#[cfg(unix)]
struct Copier<'a> {
    secrets: &'a RegisteredSecrets,
    private_owner: Option<u32>,
    limits: Limits,
    entries: usize,
    files: usize,
    bytes: u64,
    root_path: &'a Path,
    policy_root: &'a Path,
    destination: Option<&'a Path>,
    selection: Option<&'a Selection>,
    manifest: Vec<ManifestEntry>,
    exclusions: Vec<Exclusion>,
    deadline: Option<Instant>,
    exclude_build_caches: bool,
}

#[cfg(unix)]
impl Copier<'_> {
    fn walk(&mut self, dir: &File, relative: &Path, depth: usize) -> Result<()> {
        let mut walker = polaris_tools::path_policy::tree_admission::TreeWalker {
            secrets: self.secrets,
            private_owner: self.private_owner,
            limits: self.limits,
            entries: self.entries,
            files: self.files,
            bytes: self.bytes,
            root_path: self.root_path,
            policy_root: self.policy_root,
            destination: self.destination,
            selection: self.selection,
            manifest: Vec::new(),
            exclusions: Vec::new(),
            deadline: self.deadline,
            exclude_build_caches: self.exclude_build_caches,
        };
        walker.walk(dir, relative, depth)?;
        self.entries = walker.entries;
        self.files = walker.files;
        self.bytes = walker.bytes;
        self.exclusions.extend(walker.exclusions);
        self.manifest
            .extend(walker.manifest.into_iter().map(|entry| ManifestEntry {
                relative_path: entry.relative_path,
                identity: entry.identity,
                bytes: entry.bytes,
                mode: entry.mode,
                sha256: entry.sha256,
                observed_mode: entry.observed_mode,
                observed_mtime: entry.observed_mtime,
                observed_ctime: entry.observed_ctime,
            }));
        Ok(())
    }
}

/// Semantic changes; replacing a file with a directory (or vice versa) is Modified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    ModeChanged,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub relative_path: PathBuf,
    pub kind: ChangeKind,
    /// Source baseline; absent for creations.
    pub before: Option<ManifestEntry>,
    /// Scanned copy metadata and hash, never file contents; absent for deletions.
    pub after: Option<ManifestEntry>,
}

#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// Ordinary .git exclusions are informational. Other exclusions, including
    /// ancestor changes that could remove .git, make the set ineligible.
    /// Excluded entries never become deletions; consult is_applicable().
    pub exclusions: Vec<Exclusion>,
    snapshot_path: PathBuf,
    copy_identity: Identity,
    source_identity: Identity,
}

impl ChangeSet {
    pub fn is_applicable(&self) -> bool {
        self.exclusions
            .iter()
            .all(is_informational_repository_exclusion)
    }
}

fn is_informational_repository_exclusion(exclusion: &Exclusion) -> bool {
    exclusion.reason == ExclusionReason::RepositoryMetadata
        && is_repository_metadata(&exclusion.relative_path)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConflictReason {
    RootReplaced,
    AncestorReplaced,
    Missing,
    EntryChanged,
    CreationExists,
    UnexpectedEntry,
    Excluded,
    ChangedDuringScan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceConflict {
    /// Absolute source path; ancestors outside the workspace may be reported.
    pub path: PathBuf,
    pub reason: ConflictReason,
}

#[cfg(unix)]
struct Scan {
    manifest: Vec<ManifestEntry>,
    exclusions: Vec<Exclusion>,
}

#[cfg(unix)]
pub(crate) fn refreshed_secrets(secrets: &RegisteredSecrets) -> Result<RegisteredSecrets> {
    secrets.refreshed()
}

/// Shares the snapshot traversal with no destination, so this path cannot write
/// either scanned tree. Policy is checked at both physical and source locations.
#[cfg(unix)]
fn scan(
    root: &Path,
    policy_root: &Path,
    expected_identity: Identity,
    secrets: &RegisteredSecrets,
    limits: Limits,
    selection: Option<&Selection>,
    exclude_build_caches: bool,
) -> Result<Scan> {
    let (fd, ancestors) = open_root_with_ancestors(root)?;
    let before = io(root, fd.metadata())?;
    if Identity::of(&before) != expected_identity {
        return Err(error(root, ErrorKind::SourceChanged));
    }
    if secrets.denies(root, &before)
        || secrets.denies(policy_root, &before)
        || polaris_tools::path_policy::is_denied(root)
        || polaris_tools::path_policy::is_denied(policy_root)
        || is_repository_metadata(root)
        || is_repository_metadata(policy_root)
    {
        return Err(error(root, ErrorKind::InvalidPath));
    }
    let mut walker = Copier {
        secrets,
        private_owner: None,
        limits,
        entries: 0,
        files: 0,
        bytes: 0,
        root_path: root,
        policy_root,
        destination: None,
        selection,
        manifest: Vec::new(),
        exclusions: Vec::new(),
        deadline: None,
        exclude_build_caches,
    };
    walker.walk(&fd, Path::new(""), 0)?;
    let (current, current_ancestors) = open_root_with_ancestors(root)?;
    if ancestors != current_ancestors || !unchanged(&before, &io(root, current.metadata())?) {
        return Err(error(root, ErrorKind::SourceChanged));
    }
    Ok(Scan {
        manifest: walker.manifest,
        exclusions: walker.exclusions,
    })
}

#[cfg(unix)]
fn valid_relative(path: &Path, limits: Limits) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(error(path, ErrorKind::InvalidPath));
    }
    // max_depth counts directories; one final file component is permitted.
    if path.components().count().saturating_sub(1) > limits.max_depth {
        return Err(error(path, ErrorKind::Limit));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_baseline(snapshot: &Snapshot, limits: Limits) -> Result<()> {
    if snapshot
        .manifest
        .len()
        .saturating_add(snapshot.exclusions.len())
        > limits.max_entries
    {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in &snapshot.manifest {
        valid_relative(&entry.relative_path, limits)?;
        if entry.sha256.is_some() {
            files += 1;
            if files > limits.max_files
                || entry.bytes > limits.max_file_bytes
                || entry.bytes > limits.max_total_bytes.saturating_sub(bytes)
            {
                return Err(error(&entry.relative_path, ErrorKind::Limit));
            }
            bytes += entry.bytes;
        }
    }
    for excluded in &snapshot.exclusions {
        valid_relative(&excluded.relative_path, limits)?;
    }
    Ok(())
}

#[cfg(unix)]
fn same_value(before: &ManifestEntry, after: &ManifestEntry) -> bool {
    before.sha256 == after.sha256 && before.bytes == after.bytes && before.mode == after.mode
}

#[cfg(unix)]
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Call only after the child and its descendants have stopped. The result holds
/// hashes, not frozen contents; apply-back must revalidate any later content read.
/// No source or copy writes occur. No atomicity with external editors is claimed.
#[cfg(unix)]
pub fn collect_changes(
    snapshot: &Snapshot,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<ChangeSet> {
    validate_baseline(snapshot, limits)?;
    let mut secrets = refreshed_secrets(secrets)?;
    // Baseline exclusions and newly protected source identities must prevent
    // hashing their copy counterparts, whose inodes differ from the source.
    for excluded in &snapshot.exclusions {
        // .git is already denied by traversal; do not upgrade this ordinary
        // informational exclusion into a blocking registered-secret exclusion.
        if excluded.reason != ExclusionReason::RepositoryMetadata {
            secrets.reserve_path(snapshot.source_path.join(&excluded.relative_path))?;
        }
    }
    for entry in &snapshot.manifest {
        if secrets.identities().contains(&entry.identity) {
            secrets.reserve_path(snapshot.source_path.join(&entry.relative_path))?;
        }
    }
    let current = scan(
        snapshot.path(),
        &snapshot.source_path,
        snapshot.copy_identity,
        &secrets,
        limits,
        None,
        snapshot.exclude_build_caches,
    )?;
    let before: BTreeMap<_, _> = snapshot
        .manifest
        .iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    let after: BTreeMap<_, _> = current
        .manifest
        .into_iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut boundaries: BTreeMap<PathBuf, ExclusionReason> = snapshot
        .exclusions
        .iter()
        .chain(current.exclusions.iter())
        .map(|entry| (entry.relative_path.clone(), entry.reason))
        .collect();
    // Newly registered secrets must also block deletion when absent from the copy.
    for path in secrets.paths() {
        for root in [&snapshot.source_path, snapshot.path()] {
            if let Ok(relative) = path.strip_prefix(root)
                && !relative.as_os_str().is_empty()
            {
                boundaries.insert(relative.into(), ExclusionReason::RegisteredSecret);
            }
        }
    }
    for entry in &snapshot.manifest {
        if secrets.identities().contains(&entry.identity) {
            boundaries.insert(
                entry.relative_path.clone(),
                ExclusionReason::RegisteredSecret,
            );
        }
    }
    let mut blocked: BTreeMap<_, _> = current
        .exclusions
        .into_iter()
        .map(|entry| (entry.relative_path, entry.reason))
        .collect();
    let mut changes = Vec::new();
    for path in paths {
        let old = before.get(&path).copied();
        let mut new = after.get(&path).cloned();
        if let (Some(old), Some(new)) = (old, new.as_mut()) {
            // Snapshot creation adds owner rwx to directories for working access.
            // That normalization alone is not a user-requested mode change.
            if old.sha256.is_none() && new.sha256.is_none() && new.mode == old.mode | 0o700 {
                new.mode = old.mode;
            }
        }
        let kind = match (old, new.as_ref()) {
            (None, Some(_)) => ChangeKind::Created,
            (Some(_), None) => ChangeKind::Deleted,
            (Some(old), Some(new)) if same_value(old, new) => continue,
            (Some(old), Some(new)) if old.sha256 == new.sha256 && old.bytes == new.bytes => {
                ChangeKind::ModeChanged
            }
            (Some(_), Some(_)) => ChangeKind::Modified,
            (None, None) => unreachable!(),
        };
        let mut excluded = false;
        for (boundary, reason) in &boundaries {
            if overlaps(&path, boundary) {
                // Removing/changing an ancestor must not indirectly remove Git
                // metadata. Report the ancestor as blocking, not the benign .git.
                let blocked_path = if *reason == ExclusionReason::RepositoryMetadata
                    && boundary.starts_with(&path)
                    && boundary != &path
                {
                    &path
                } else {
                    boundary
                };
                blocked.insert(blocked_path.clone(), *reason);
                excluded = true;
            }
        }
        if excluded {
            continue;
        }
        if changes.len() >= limits.max_entries {
            return Err(error(&path, ErrorKind::Limit));
        }
        changes.push(Change {
            relative_path: path,
            kind,
            before: old.cloned(),
            after: new,
        });
    }
    if changes.len().saturating_add(blocked.len()) > limits.max_entries {
        return Err(error(snapshot.path(), ErrorKind::Limit));
    }
    Ok(ChangeSet {
        changes,
        exclusions: blocked
            .into_iter()
            .map(|(relative_path, reason)| Exclusion {
                relative_path,
                reason,
            })
            .collect(),
        snapshot_path: snapshot.path().into(),
        copy_identity: snapshot.copy_identity,
        source_identity: snapshot.source_identity,
    })
}

#[cfg(unix)]
fn source_roots_conflicts(snapshot: &Snapshot) -> Result<Vec<SourceConflict>> {
    let (root, ancestors) = match open_root_with_ancestors(&snapshot.source_path) {
        Ok(opened) => opened,
        Err(e) if e.kind == ErrorKind::Io => {
            return Ok(vec![SourceConflict {
                reason: if e.path == snapshot.source_path {
                    ConflictReason::RootReplaced
                } else {
                    ConflictReason::AncestorReplaced
                },
                path: e.path,
            }]);
        }
        Err(e) => return Err(e),
    };
    if Identity::of(&io(&snapshot.source_path, root.metadata())?) != snapshot.source_identity {
        return Ok(vec![SourceConflict {
            path: snapshot.source_path.clone(),
            reason: ConflictReason::RootReplaced,
        }]);
    }
    Ok(snapshot
        .source_ancestors
        .iter()
        .zip(ancestors.iter())
        .filter(|(before, after)| before != after)
        .map(|(before, _)| SourceConflict {
            path: before.0.clone(),
            reason: ConflictReason::AncestorReplaced,
        })
        .collect())
}

/// Advisory conflict check only. Hashes changed paths and inspects their
/// ancestors; directories being deleted/replaced are recursively checked for
/// unexpected children. Unrelated file contents are not read. Even an empty
/// result is not an atomic compare-and-swap guarantee for a later apply-back.
#[cfg(unix)]
pub fn check_source_conflicts(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<Vec<SourceConflict>> {
    validate_baseline(snapshot, limits)?;
    if changes.snapshot_path != snapshot.path()
        || changes.copy_identity != snapshot.copy_identity
        || changes.source_identity != snapshot.source_identity
    {
        return Err(error(&snapshot.source_path, ErrorKind::InvalidPath));
    }
    if changes
        .changes
        .len()
        .saturating_add(changes.exclusions.len())
        > limits.max_entries
    {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    let roots = source_roots_conflicts(snapshot)?;
    if !roots.is_empty() {
        return Ok(roots);
    }
    let mut selection = Selection::default();
    let baseline: BTreeMap<_, _> = snapshot
        .manifest
        .iter()
        .map(|entry| (entry.relative_path.clone(), entry))
        .collect();
    let mut conflicts = BTreeSet::new();
    for exclusion in &changes.exclusions {
        valid_relative(&exclusion.relative_path, limits)?;
        if is_informational_repository_exclusion(exclusion) {
            continue;
        }
        conflicts.insert((
            snapshot.source_path.join(&exclusion.relative_path),
            ConflictReason::Excluded,
        ));
    }
    for change in &changes.changes {
        valid_relative(&change.relative_path, limits)?;
        for ancestor in change
            .relative_path
            .ancestors()
            .filter(|path| !path.as_os_str().is_empty())
        {
            selection.exact.insert(ancestor.into());
        }
        if baseline
            .get(&change.relative_path)
            .is_some_and(|entry| entry.sha256.is_none())
            && change
                .after
                .as_ref()
                .is_none_or(|entry| entry.sha256.is_some())
        {
            selection.recursive.insert(change.relative_path.clone());
        }
    }
    if selection.exact.len() > limits.max_entries {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    if !selection.exact.is_empty() {
        let secrets = refreshed_secrets(secrets)?;
        let current = match scan(
            &snapshot.source_path,
            &snapshot.source_path,
            snapshot.source_identity,
            &secrets,
            limits,
            Some(&selection),
            snapshot.exclude_build_caches,
        ) {
            Ok(current) => current,
            Err(e) if e.kind == ErrorKind::SourceChanged => {
                return Ok(vec![SourceConflict {
                    path: e.path,
                    reason: ConflictReason::ChangedDuringScan,
                }]);
            }
            Err(e) => return Err(e),
        };
        let present: BTreeMap<_, _> = current
            .manifest
            .iter()
            .map(|entry| (entry.relative_path.clone(), entry))
            .collect();
        let paths: BTreeSet<_> = selection
            .exact
            .iter()
            .cloned()
            .chain(
                baseline
                    .keys()
                    .filter(|path| selection.includes(path))
                    .cloned(),
            )
            .chain(present.keys().cloned())
            .collect();
        for path in paths {
            if current
                .exclusions
                .iter()
                .any(|excluded| path.starts_with(&excluded.relative_path))
            {
                conflicts.insert((snapshot.source_path.join(&path), ConflictReason::Excluded));
                continue;
            }
            let reason = match (baseline.get(&path), present.get(&path)) {
                (Some(old), Some(now))
                    if old.identity == now.identity
                        && old.observed_mode == now.observed_mode
                        && same_value(old, now) =>
                {
                    None
                }
                (Some(old), Some(now))
                    if old.sha256.is_none()
                        && (old.identity != now.identity || now.sha256.is_some()) =>
                {
                    Some(ConflictReason::AncestorReplaced)
                }
                (Some(_), Some(_)) => Some(ConflictReason::EntryChanged),
                (Some(_), None) => Some(ConflictReason::Missing),
                (None, Some(_)) if selection.exact.contains(&path) => {
                    Some(ConflictReason::CreationExists)
                }
                (None, Some(_)) => Some(ConflictReason::UnexpectedEntry),
                (None, None) => None,
            };
            if let Some(reason) = reason {
                conflicts.insert((snapshot.source_path.join(path), reason));
            }
        }
        for excluded in current.exclusions {
            conflicts.insert((
                snapshot.source_path.join(excluded.relative_path),
                ConflictReason::Excluded,
            ));
        }
    }
    let roots = source_roots_conflicts(snapshot)?;
    conflicts.extend(
        roots
            .into_iter()
            .map(|conflict| (conflict.path, conflict.reason)),
    );
    if conflicts.len() > limits.max_entries {
        return Err(error(&snapshot.source_path, ErrorKind::Limit));
    }
    Ok(conflicts
        .into_iter()
        .map(|(path, reason)| SourceConflict { path, reason })
        .collect())
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(not(unix))]
pub fn snapshot(source: &Path, _secrets: &RegisteredSecrets, _limits: Limits) -> Result<Snapshot> {
    Err(error(source, ErrorKind::ProtectionUnavailable))
}

#[cfg(not(unix))]
pub fn protected_snapshot(source: &Path, _limits: Limits) -> Result<Snapshot> {
    Err(error(source, ErrorKind::ProtectionUnavailable))
}

#[cfg(not(unix))]
pub fn collect_changes(
    snapshot: &Snapshot,
    _secrets: &RegisteredSecrets,
    _limits: Limits,
) -> Result<ChangeSet> {
    Err(error(
        &snapshot.source_path,
        ErrorKind::ProtectionUnavailable,
    ))
}

#[cfg(not(unix))]
pub fn protected_collect_changes(snapshot: &Snapshot, _limits: Limits) -> Result<ChangeSet> {
    Err(error(
        &snapshot.source_path,
        ErrorKind::ProtectionUnavailable,
    ))
}

#[cfg(not(unix))]
pub fn check_source_conflicts(
    snapshot: &Snapshot,
    _changes: &ChangeSet,
    _secrets: &RegisteredSecrets,
    _limits: Limits,
) -> Result<Vec<SourceConflict>> {
    Err(error(
        &snapshot.source_path,
        ErrorKind::ProtectionUnavailable,
    ))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_entries: 8,
            max_files: 4,
            max_file_bytes: 128,
            max_total_bytes: 256,
            max_depth: 2,
        }
    }

    #[test]
    fn windows_snapshot_refuses_before_source_or_auth_access() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("existing.txt");
        std::fs::write(&marker, b"source bytes").unwrap();
        let missing = root.path().join("missing");
        for source in [root.path(), missing.as_path()] {
            let error = protected_snapshot(source, limits()).unwrap_err();
            assert_eq!(error.kind, ErrorKind::ProtectionUnavailable);
            assert_eq!(error.path, source);
        }
        assert_eq!(std::fs::read(&marker).unwrap(), b"source bytes");
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn windows_snapshot_refuses_scan_and_keeps_real_path_getters() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let tmp = root.path().join("tmp");
        for path in [&workspace, &home, &tmp] {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("existing.txt"), b"fixture bytes").unwrap();
        }
        let identity = Identity {
            device: 7,
            inode: 9,
        };
        let snapshot = Snapshot {
            _root: root,
            workspace: workspace.clone(),
            home: home.clone(),
            tmp: tmp.clone(),
            copy_identity: identity,
            source_ancestors: Vec::new(),
            source_path: workspace.clone(),
            source_identity: identity,
            manifest: Vec::new(),
            exclusions: Vec::new(),
            exclude_build_caches: false,
        };
        assert_eq!(snapshot.path(), workspace);
        assert_eq!(snapshot.scratch_home(), home);
        assert_eq!(snapshot.scratch_tmp(), tmp);
        assert_eq!(
            protected_collect_changes(&snapshot, limits())
                .unwrap_err()
                .kind,
            ErrorKind::ProtectionUnavailable
        );
        for path in [&workspace, &home, &tmp] {
            assert_eq!(
                std::fs::read(path.join("existing.txt")).unwrap(),
                b"fixture bytes"
            );
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 1);
        }
    }

    #[test]
    fn windows_change_set_preserves_informational_exclusion_rule() {
        let identity = Identity {
            device: 7,
            inode: 9,
        };
        let mut changes = ChangeSet {
            changes: Vec::new(),
            exclusions: Vec::new(),
            snapshot_path: PathBuf::from(r"C:\fixture"),
            copy_identity: identity,
            source_identity: identity,
        };
        assert!(changes.is_applicable());
        changes.exclusions.push(Exclusion {
            relative_path: PathBuf::from(".GIT/config"),
            reason: ExclusionReason::RepositoryMetadata,
        });
        assert!(changes.is_applicable());
        changes.exclusions[0].relative_path = PathBuf::from("src");
        assert!(!changes.is_applicable());
        changes.exclusions[0].relative_path = PathBuf::from(".git/config");
        changes.exclusions[0].reason = ExclusionReason::RegisteredSecret;
        assert!(!changes.is_applicable());
    }
}
