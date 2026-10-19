//! Unix file-only apply-back with retained originals and a durable intent journal.
//! The caller stops snapshot writers and serializes secret registration for the
//! entire call (including candidate staging and capture validation).
//! Recovery directories are deliberately never garbage-collected. This is not a
//! multi-file transaction or a CAS against non-cooperating editors/open file FDs.

use crate::isolated_workspace::{
    self as copy, Change, ChangeSet, Limits, ManifestEntry, RegisteredSecrets, Snapshot,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, Metadata, Permissions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

mod report;
pub use report::{
    ApplyFailure, ApplyReport, EntryReport, FailureKind, MAX_APPLY_REPORT_BYTES, Phase,
    decode_apply_report,
};
use report::{bounded_evidence, decode_exact, fail};

/// Header entry order follows the existing v1 JSON-map writer. These types
/// retain the hash-array and tuple representation used by recovery artifacts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalAfter {
    pub bytes: u64,
    pub hash: [u8; 32],
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalBefore {
    pub bytes: u64,
    pub device: u64,
    pub hash: [u8; 32],
    pub inode: u64,
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalChange {
    pub after: Option<JournalAfter>,
    pub before: Option<JournalBefore>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalHeaderV2 {
    pub version: u8,
    pub operation_key: crate::management_effect::ManagementEffectHashV1,
    pub intent_descriptor_hash: crate::management_effect::ManagementEffectHashV1,
    pub recovery_parent_identity: crate::apply_intent::IdentityV1,
    pub source: PathBuf,
    pub source_chain: Vec<(PathBuf, (u64, u64))>,
    pub recovery_chain: Vec<(PathBuf, (u64, u64))>,
    pub entries: Vec<JournalChange>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalHeaderV1 {
    pub entries: Vec<JournalChange>,
    pub recovery_chain: Vec<(PathBuf, (u64, u64))>,
    pub source: PathBuf,
    pub source_chain: Vec<(PathBuf, (u64, u64))>,
    pub version: u8,
}

#[derive(Debug)]
pub enum SavedJournalHeader {
    V1(JournalHeaderV1),
    V2(JournalHeaderV2),
}

#[derive(Debug)]
pub struct SavedJournal {
    pub header: SavedJournalHeader,
    pub entries: Vec<EntryReport>,
}

fn evidence_hash(domain: &[u8], bytes: &[u8]) -> crate::management_effect::ManagementEffectHashV1 {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    let hex: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    crate::management_effect::ManagementEffectHashV1::from_hex(&hex)
        .expect("SHA-256 produces a fixed-width lowercase hash")
}

fn validate_journal_paths(
    source: &Path,
    source_chain: &[(PathBuf, (u64, u64))],
    recovery_chain: &[(PathBuf, (u64, u64))],
    entries: &[JournalChange],
) -> Result<()> {
    let absolute = |path: &Path| {
        path.to_str().is_some()
            && path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
    };
    if !absolute(source)
        || source_chain.last().map(|e| e.0.as_path()) != Some(source)
        || source_chain.is_empty()
        || recovery_chain.len() < 2
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    for chain in [source_chain, recovery_chain] {
        if chain.first().map(|e| e.0.as_path()) != Some(Path::new("/"))
            || chain.iter().any(|(p, _)| !absolute(p))
            || chain
                .windows(2)
                .any(|pair| pair[1].0.parent() != Some(pair[0].0.as_path()))
        {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
    }
    let mut previous: Option<&[u8]> = None;
    for entry in entries {
        let path = entry
            .path
            .to_str()
            .ok_or_else(|| fail(FailureKind::UnsupportedEntry))?;
        crate::apply_intent::validate_relative_path(path)
            .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
        if previous.is_some_and(|p| p >= path.as_bytes())
            || (entry.before.is_none() && entry.after.is_none())
            || entry.before.as_ref().is_some_and(|e| e.mode > 0o7777)
            || entry.after.as_ref().is_some_and(|e| e.mode > 0o777)
        {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        previous = Some(path.as_bytes());
    }
    Ok(())
}

impl JournalHeaderV2 {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        validate_journal_paths(
            &self.source,
            &self.source_chain,
            &self.recovery_chain,
            &self.entries,
        )?;
        let parent = self.recovery_chain[self.recovery_chain.len() - 2].1;
        let child = self.recovery_chain.last().unwrap().0.file_name();
        let expected_child = crate::apply_intent::recovery_child(self.operation_key)
            .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
        if self.version != 2
            || parent
                != (
                    self.recovery_parent_identity.device,
                    self.recovery_parent_identity.inode,
                )
            || child != Some(OsStr::new(&expected_child))
            || self
                .intent_descriptor_hash
                .as_hex()
                .bytes()
                .all(|b| b == b'0')
        {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        serde_json::to_vec(self).map_err(|_| fail(FailureKind::InvalidChangeSet))
    }

    pub fn header_hash(&self) -> Result<crate::management_effect::ManagementEffectHashV1> {
        Ok(evidence_hash(
            b"polaris-workspace-journal-v2-header-v1\0",
            &self.canonical_bytes()?,
        ))
    }
}

/// Decoding is inspection only. A v2 header alone never grants permission to
/// repair or apply; the shared owner must join its descriptor and effect chain.
pub fn decode_journal(bytes: &[u8], maximum: u64) -> Result<SavedJournal> {
    bounded_evidence(bytes, maximum)?;
    if !bytes.ends_with(b"\n") {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    let mut lines = bytes[..bytes.len() - 1].split(|b| *b == b'\n');
    let first = lines
        .next()
        .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
    let version: serde_json::Value =
        serde_json::from_slice(first).map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    let header = match version.get("version").and_then(|v| v.as_u64()) {
        Some(1) => {
            let header: JournalHeaderV1 = decode_exact(first)?;
            validate_journal_paths(
                &header.source,
                &header.source_chain,
                &header.recovery_chain,
                &header.entries,
            )?;
            SavedJournalHeader::V1(header)
        }
        Some(2) => {
            let header: JournalHeaderV2 = decode_exact(first)?;
            header.canonical_bytes()?;
            SavedJournalHeader::V2(header)
        }
        _ => return Err(fail(FailureKind::InvalidChangeSet)),
    };
    let entries = lines
        .map(decode_exact)
        .collect::<Result<Vec<EntryReport>>>()?;
    Ok(SavedJournal { header, entries })
}

pub fn journal_tip_hash(
    bytes: &[u8],
    maximum: u64,
) -> Result<crate::management_effect::ManagementEffectHashV1> {
    match decode_journal(bytes, maximum)?.header {
        SavedJournalHeader::V2(_) => Ok(evidence_hash(
            b"polaris-workspace-journal-v2-tip-v1\0",
            bytes,
        )),
        SavedJournalHeader::V1(_) => Err(fail(FailureKind::InvalidChangeSet)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceCapacityUpperBound {
    pub journal_bytes: u64,
    pub report_bytes: u64,
}

struct EncodedLength(u64);
impl Write for EncodedLength {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("workspace evidence length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encoded_length(value: &impl Serialize) -> Result<u64> {
    let mut length = EncodedLength(0);
    serde_json::to_writer(&mut length, value).map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    Ok(length.0)
}

fn journal_hash_array(hash: crate::management_effect::ManagementEffectHashV1) -> [u8; 32] {
    let hex = hash.as_hex();
    let mut result = [0; 32];
    for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        result[i] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    result
}

/// Admission uses known paths and request metadata. Only future syscall
/// identities, booleans, phase/failure variants and integer widths use maxima.
/// Lengths are counted with the same encoders as the actual artifacts.
pub fn capacity_upper_bound(
    request: &crate::apply_intent::RequestV1,
    source: &Path,
    source_chain: &[(PathBuf, (u64, u64))],
    recovery_parent: &Path,
    recovery_chain: &[(PathBuf, (u64, u64))],
    operation_key: crate::management_effect::ManagementEffectHashV1,
) -> Result<WorkspaceCapacityUpperBound> {
    use crate::apply_intent::CanonicalApplyEvidence;
    request
        .validate()
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if encoded_length(request)? > crate::apply_intent::MAX_ARTIFACT_BYTES {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    if source_chain.last().map(|e| e.1)
        != Some((
            request.source_identity.device,
            request.source_identity.inode,
        ))
        || recovery_chain.last().map(|e| e.0.as_path()) != Some(recovery_parent)
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    let parent_identity = recovery_chain
        .last()
        .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?
        .1;
    let child = crate::apply_intent::recovery_child(operation_key)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    let recovery_path = recovery_parent.join(child);
    let mut child_chain = recovery_chain.to_vec();
    child_chain.push((recovery_path.clone(), (parent_identity.0, u64::MAX)));
    let header = JournalHeaderV2 {
        version: 2,
        operation_key,
        // The eventual descriptor hash has this fixed width. It is not
        // persisted or used as authorization evidence by this calculation.
        intent_descriptor_hash: crate::management_effect::ManagementEffectHashV1::from_hex(
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        )
        .expect("fixed-width capacity hash"),
        recovery_parent_identity: crate::apply_intent::IdentityV1 {
            device: parent_identity.0,
            inode: parent_identity.1,
        },
        source: source.into(),
        source_chain: source_chain.to_vec(),
        recovery_chain: child_chain,
        entries: request
            .entries
            .iter()
            .map(|entry| JournalChange {
                after: entry.after.as_ref().map(|e| JournalAfter {
                    bytes: e.bytes,
                    hash: journal_hash_array(e.hash),
                    mode: e.mode,
                }),
                before: entry.before.as_ref().map(|e| JournalBefore {
                    bytes: e.bytes,
                    device: e.device,
                    hash: journal_hash_array(e.hash),
                    inode: e.inode,
                    mode: e.observed_mode,
                }),
                path: entry.path.clone().into(),
            })
            .collect(),
    };
    let mut journal_bytes = (header.canonical_bytes()?.len() as u64)
        .checked_add(1)
        .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
    let report = ApplyReport {
        source_path: source.into(),
        recovery_path: Some(recovery_path),
        recovery_identity: Some((parent_identity.0, u64::MAX)),
        recovery_directory: None,
        entries: Vec::new(),
        failure: Some(ApplyFailure {
            kind: FailureKind::ProtectionUnavailable,
            entry: Some(usize::MAX),
            os_error: Some(i32::MIN),
        }),
    };
    let mut report_bytes = encoded_length(&report)?;
    if report_bytes > MAX_APPLY_REPORT_BYTES {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    for (i, change) in request.entries.iter().enumerate() {
        let mut parent_chain = source_chain.to_vec();
        let mut path = source.to_path_buf();
        for component in Path::new(&change.path).parent().unwrap().components() {
            path.push(component.as_os_str());
            parent_chain.push((path.clone(), (u64::MAX, u64::MAX)));
        }
        let mut row = EntryReport {
            relative_path: change.path.clone().into(),
            source_parent_chain: parent_chain,
            phase: if change.before.is_some() {
                Phase::PreserveIntent
            } else {
                Phase::InstallIntent
            },
            old_retained: false,
            installed: false,
            deleted: false,
            restored: false,
            old_name: format!("old-{i}"),
            candidate_name: change.after.as_ref().map(|_| format!("candidate-{i}")),
            install_name: change.after.as_ref().map(|_| format!("install-{i}")),
            candidate_identity: change.after.as_ref().map(|_| (parent_identity.0, u64::MAX)),
            install_identity: change.after.as_ref().map(|_| (parent_identity.0, u64::MAX)),
        };
        // A replacement can reach rollback after InstallIntent. Delete can
        // roll back after verification; create has no original to restore.
        let phases: &[Phase] = match (change.before.is_some(), change.after.is_some()) {
            (true, true) => &[
                Phase::Prepared,
                Phase::PreserveIntent,
                Phase::Preserved,
                Phase::InstallIntent,
                Phase::RestoreIntent,
                Phase::Restored,
            ],
            (true, false) => &[
                Phase::Prepared,
                Phase::PreserveIntent,
                Phase::Preserved,
                Phase::RestoreIntent,
                Phase::Restored,
            ],
            (false, true) => &[Phase::Prepared, Phase::InstallIntent, Phase::Installed],
            (false, false) => return Err(fail(FailureKind::InvalidChangeSet)),
        };
        let report_phase = row.phase;
        for phase in phases {
            row.phase = *phase;
            journal_bytes = journal_bytes
                .checked_add(encoded_length(&row)?)
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
        }
        row.phase = report_phase;
        // Count the rows inside the existing [] and their commas. Do not
        // retain every repeated ancestor chain just to reject an oversized
        // report after constructing the entire object.
        report_bytes = report_bytes
            .checked_add(encoded_length(&row)?)
            .and_then(|n| n.checked_add(u64::from(i != 0)))
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
        if report_bytes > MAX_APPLY_REPORT_BYTES {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
    }
    Ok(WorkspaceCapacityUpperBound {
        journal_bytes,
        report_bytes,
    })
}

impl ApplyReport {
    pub fn evidence_hash(&self) -> Result<crate::management_effect::ManagementEffectHashV1> {
        Ok(evidence_hash(
            b"polaris-workspace-apply-report-v1\0",
            &self.canonical_bytes()?,
        ))
    }

    /// The supplied FD is an already pinned, owner-private recovery directory.
    /// Failures retain a newly created artifact, including any partial prefix.
    pub fn publish_exact(
        &self,
        directory: &File,
    ) -> Result<crate::management_effect::ManagementEffectHashV1> {
        let bytes = self.canonical_bytes()?;
        let expected = self
            .recovery_identity
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
        private_directory(directory, expected)?;
        let name = OsStr::new("apply-report.v1.json");
        let mut file = open_at(directory, name, false, true)?;
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(io)?;
        let metadata = file.metadata().map_err(io)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        file.write_all(&bytes).map_err(io)?;
        file.sync_data().map_err(io)?;
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        let mut reread = Vec::new();
        (&mut file)
            .take(bytes.len() as u64 + 1)
            .read_to_end(&mut reread)
            .map_err(io)?;
        if reread != bytes {
            return Err(fail(FailureKind::Conflict));
        }
        decode_apply_report(&reread)?;
        let entry = open_at(directory, name, false, false)?;
        let current = file.metadata().map_err(io)?;
        if identity(&entry.metadata().map_err(io)?) != identity(&metadata)
            || current.len() != bytes.len() as u64
            || current.mode() & 0o7777 != 0o600
            || current.nlink() != 1
            || current.uid() != unsafe { libc::geteuid() }
            || !stable(&current, &entry.metadata().map_err(io)?)
        {
            return Err(fail(FailureKind::AncestorChanged));
        }
        private_directory(directory, expected)?;
        directory.sync_all().map_err(io)?;
        Ok(evidence_hash(
            b"polaris-workspace-apply-report-v1\0",
            &bytes,
        ))
    }
}

type Result<T> = std::result::Result<T, ApplyFailure>;
fn io(error: std::io::Error) -> ApplyFailure {
    ApplyFailure {
        kind: if error.raw_os_error() == Some(libc::EXDEV) {
            FailureKind::CrossDevice
        } else {
            FailureKind::Io
        },
        entry: None,
        os_error: error.raw_os_error(),
    }
}
fn cstr(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| fail(FailureKind::InvalidChangeSet))
}
fn identity(m: &Metadata) -> (u64, u64) {
    (m.dev(), m.ino())
}
fn stable(a: &Metadata, b: &Metadata) -> bool {
    identity(a) == identity(b)
        && a.len() == b.len()
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn open_at(parent: &File, name: &OsStr, directory: bool, create: bool) -> Result<File> {
    let name = cstr(name)?;
    let flags = libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        }
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: live dirfd, single validated name, NUL-terminated argument.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Trusted operation-specific recovery parent. The path is provenance only;
/// the owned descriptor, bound to the approved identity, selects the directory.
#[derive(Debug)]
pub struct RecoveryParent {
    directory: File,
    expected: (u64, u64),
    provenance: PathBuf,
}
impl RecoveryParent {
    /// The caller keeps this directory outside child permissions and binds its
    /// identity in the durable intent before dispatch. No auth lock is acquired.
    pub fn pin(
        directory: &File,
        expected: (u64, u64),
        provenance: &Path,
    ) -> std::result::Result<Self, ApplyFailure> {
        let pinned = Self {
            directory: directory.try_clone().map_err(io)?,
            expected,
            provenance: provenance.into(),
        };
        pinned.validate_identity()?;
        Ok(pinned)
    }
    /// Recheck the held FD's identity, directory type, ownership and private mode
    /// immediately before committing intent. Never reopens the provenance path,
    /// writes, or grants authorization. Apply also checks filesystem and scope.
    pub fn validate_identity(&self) -> std::result::Result<(), ApplyFailure> {
        private_directory(&self.directory, self.expected)
    }

    /// The expected (device, inode) bound at pin time; not a fresh validation.
    pub fn identity(&self) -> (u64, u64) {
        self.expected
    }
    pub(crate) fn retained_directory(&self) -> std::io::Result<File> {
        self.directory.try_clone()
    }
    /// Original informational path, shared by approval and report evidence.
    /// It may have been renamed; only the held FD selects the recovery directory.
    pub fn provenance(&self) -> &Path {
        &self.provenance
    }

    /// Metadata-only pre-intent validation of the pinned recovery scope. Apply
    /// repeats its checks; this does not grant write permission or lock paths.
    pub fn validate_for_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        self.validate_identity()?;
        let source = Anchor::open(&snapshot.source_path)?;
        let metadata = source.dir.metadata().map_err(io)?;
        if identity(&metadata)
            != (
                snapshot.source_identity.device,
                snapshot.source_identity.inode,
            )
        {
            return Err(fail(FailureKind::AncestorChanged));
        }
        if metadata.dev() != self.expected.0 {
            return Err(fail(FailureKind::CrossDevice));
        }
        let copy_parent = Anchor::open(
            snapshot
                .path()
                .parent()
                .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?,
        )?;
        outside(&self.directory, &source.dir)?;
        outside(&self.directory, &copy_parent.dir)?;
        source.check()?;
        copy_parent.check()
    }
}

fn private_directory(directory: &File, expected: (u64, u64)) -> Result<()> {
    let metadata = directory.metadata().map_err(io)?;
    if identity(&metadata) != expected {
        return Err(fail(FailureKind::AncestorChanged));
    }
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7077 != 0
        || metadata.mode() & 0o700 != 0o700
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    Ok(())
}

/// Compare actual ancestor identities, never the informational recovery path.
fn outside(directory: &File, forbidden: &File) -> Result<()> {
    let forbidden = identity(&forbidden.metadata().map_err(io)?);
    let mut current = directory.try_clone().map_err(io)?;
    for _ in 0..1024 {
        let here = identity(&current.metadata().map_err(io)?);
        if here == forbidden {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        let parent = open_at(&current, OsStr::new(".."), true, false)?;
        if identity(&parent.metadata().map_err(io)?) == here {
            return Ok(());
        }
        current = parent;
    }
    Err(fail(FailureKind::InvalidChangeSet))
}

#[derive(Debug)]
struct Anchor {
    path: PathBuf,
    dir: File,
    chain: Vec<(PathBuf, (u64, u64))>,
    pinned: Option<(u64, u64)>,
}

/// A retained child and its exact v2 header, pinned before effect dispatch.
/// This evidence token carries no management-effect or source-write authority.
#[derive(Debug)]
pub struct ReservedRecoveryV2 {
    parent: RecoveryParent,
    anchor: Anchor,
    header: JournalHeaderV2,
}

impl ReservedRecoveryV2 {
    pub(crate) fn retained_parent_directory(&self) -> std::io::Result<File> {
        self.parent.retained_directory()
    }
    pub fn header(&self) -> &JournalHeaderV2 {
        &self.header
    }
    pub fn directory(&self) -> &File {
        &self.anchor.dir
    }
    pub fn path(&self) -> &Path {
        &self.anchor.path
    }

    pub fn inspect_journal(&self, maximum: u64) -> Result<Vec<u8>> {
        self.parent.validate_identity()?;
        self.anchor.check()?;
        let name = self
            .anchor
            .path
            .file_name()
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
        let child = open_at(&self.parent.directory, name, true, false)?;
        if identity(&child.metadata().map_err(io)?)
            != identity(&self.anchor.dir.metadata().map_err(io)?)
        {
            return Err(fail(FailureKind::AncestorChanged));
        }
        let bytes = read_owned_artifact(&self.anchor.dir, OsStr::new("journal.jsonl"), maximum)?;
        match decode_journal(&bytes, maximum)?.header {
            SavedJournalHeader::V2(ref header) if header == &self.header => Ok(bytes),
            _ => Err(fail(FailureKind::Conflict)),
        }
    }
}

fn read_owned_artifact(directory: &File, name: &OsStr, maximum: u64) -> Result<Vec<u8>> {
    let mut file = open_at(directory, name, false, false)?;
    let before = file.metadata().map_err(io)?;
    if !before.is_file()
        || before.nlink() != 1
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o7777 != 0o600
        || before.len() > maximum
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    bounded_evidence(&bytes, maximum)?;
    let entry = open_at(directory, name, false, false)?;
    if !stable(&before, &file.metadata().map_err(io)?)
        || !stable(&before, &entry.metadata().map_err(io)?)
        || bytes.len() as u64 != before.len()
    {
        return Err(fail(FailureKind::AncestorChanged));
    }
    Ok(bytes)
}

fn journal_changes(changes: &[Change]) -> Result<Vec<JournalChange>> {
    changes
        .iter()
        .map(|c| {
            Ok(JournalChange {
                after: c
                    .after
                    .as_ref()
                    .map(|e| {
                        Ok::<_, ApplyFailure>(JournalAfter {
                            bytes: e.bytes,
                            hash: e
                                .sha256
                                .ok_or_else(|| fail(FailureKind::UnsupportedEntry))?,
                            mode: e.mode,
                        })
                    })
                    .transpose()?,
                before: c
                    .before
                    .as_ref()
                    .map(|e| {
                        Ok::<_, ApplyFailure>(JournalBefore {
                            bytes: e.bytes,
                            device: e.identity.device,
                            hash: e
                                .sha256
                                .ok_or_else(|| fail(FailureKind::UnsupportedEntry))?,
                            inode: e.identity.inode,
                            mode: e.observed_mode(),
                        })
                    })
                    .transpose()?,
                path: c.relative_path.clone(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_reserved_recovery_v2(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    parent: &RecoveryParent,
    operation_key: crate::management_effect::ManagementEffectHashV1,
    descriptor_hash: crate::management_effect::ManagementEffectHashV1,
) -> Result<ReservedRecoveryV2> {
    parent.validate_for_snapshot(snapshot)?;
    let source = Anchor::open(&snapshot.source_path)?;
    let owner = Anchor::open(parent.provenance())?;
    if identity(&owner.dir.metadata().map_err(io)?) != parent.identity() {
        return Err(fail(FailureKind::AncestorChanged));
    }
    let fresh = validate_changes(snapshot, changes, secrets, limits)?;
    let child = crate::apply_intent::recovery_child(operation_key)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    let path = parent.provenance().join(&child);
    let mut chain = owner.chain.clone();
    chain.push((path.clone(), (parent.expected.0, u64::MAX)));
    let mut header = JournalHeaderV2 {
        version: 2,
        operation_key,
        intent_descriptor_hash: descriptor_hash,
        recovery_parent_identity: crate::apply_intent::IdentityV1 {
            device: parent.expected.0,
            inode: parent.expected.1,
        },
        source: snapshot.source_path.clone(),
        source_chain: source.chain.clone(),
        recovery_chain: chain,
        entries: journal_changes(&fresh.changes)?,
    };
    // Invalid wire/bindings are refused before child creation.
    header.canonical_bytes()?;
    source.check()?;
    owner.check()?;
    parent.validate_identity()?;
    let name = cstr(OsStr::new(&child))?;
    if unsafe { libc::mkdirat(parent.directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io(std::io::Error::last_os_error()));
    }
    let directory = open_at(&parent.directory, OsStr::new(&child), true, false)?;
    let expected = identity(&directory.metadata().map_err(io)?);
    private_directory(&directory, expected)?;
    header.recovery_chain.last_mut().unwrap().1 = expected;
    let anchor = Anchor {
        path,
        dir: directory,
        chain: header.recovery_chain.clone(),
        pinned: Some(expected),
    };
    let mut bytes = header.canonical_bytes()?;
    bytes.push(b'\n');
    let mut log = open_at(&anchor.dir, OsStr::new("journal.jsonl"), false, true)?;
    log.set_permissions(Permissions::from_mode(0o600))
        .map_err(io)?;
    log.write_all(&bytes).map_err(io)?;
    log.sync_data().map_err(io)?;
    log.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut reread = Vec::new();
    (&mut log)
        .take(bytes.len() as u64 + 1)
        .read_to_end(&mut reread)
        .map_err(io)?;
    if reread != bytes {
        return Err(fail(FailureKind::Conflict));
    }
    decode_journal(&reread, bytes.len() as u64)?;
    if read_owned_artifact(&anchor.dir, OsStr::new("journal.jsonl"), bytes.len() as u64)? != bytes {
        return Err(fail(FailureKind::Conflict));
    }
    anchor.dir.sync_all().map_err(io)?;
    parent.directory.sync_all().map_err(io)?;
    let linked = open_at(&anchor.dir, OsStr::new("journal.jsonl"), false, false)?;
    if !stable(
        &log.metadata().map_err(io)?,
        &linked.metadata().map_err(io)?,
    ) {
        return Err(fail(FailureKind::AncestorChanged));
    }
    source.check()?;
    owner.check()?;
    parent.validate_identity()?;
    Ok(ReservedRecoveryV2 {
        parent: RecoveryParent::pin(&parent.directory, parent.expected, &parent.provenance)?,
        anchor,
        header,
    })
}

fn require_header_only_entries(directory: &File) -> Result<()> {
    let raw = open_at(directory, OsStr::new("."), true, false)?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(raw) };
        return Err(io(error));
    }
    let result = (|| {
        let mut journal_seen = false;
        loop {
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(io(error));
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if name != b"journal.jsonl" || journal_seen {
                return Err(fail(FailureKind::Conflict));
            }
            journal_seen = true;
        }
        if !journal_seen {
            return Err(fail(FailureKind::Conflict));
        }
        Ok(())
    })();
    if unsafe { libc::closedir(stream) } != 0 && result.is_ok() {
        return Err(io(std::io::Error::last_os_error()));
    }
    result
}

/// Read-only reopening is not authorization or repair. The owner must join the
/// returned header/rows to its saved intent and management evidence.
pub fn inspect_reserved_recovery_v2(
    parent: &RecoveryParent,
    operation_key: crate::management_effect::ManagementEffectHashV1,
    descriptor_hash: crate::management_effect::ManagementEffectHashV1,
    maximum: u64,
) -> Result<Option<ReservedRecoveryV2>> {
    parent.validate_identity()?;
    let child = crate::apply_intent::recovery_child(operation_key)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    let directory = match open_at(&parent.directory, OsStr::new(&child), true, false) {
        Ok(file) => file,
        Err(e) if e.os_error == Some(libc::ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let expected = identity(&directory.metadata().map_err(io)?);
    private_directory(&directory, expected)?;
    let bytes = read_owned_artifact(&directory, OsStr::new("journal.jsonl"), maximum)?;
    let SavedJournalHeader::V2(header) = decode_journal(&bytes, maximum)?.header else {
        return Err(fail(FailureKind::Conflict));
    };
    if header.operation_key != operation_key
        || header.intent_descriptor_hash != descriptor_hash
        || (
            header.recovery_parent_identity.device,
            header.recovery_parent_identity.inode,
        ) != parent.expected
        || header.recovery_chain.last().map(|e| e.1) != Some(expected)
        || header.recovery_chain.last().map(|e| e.0.as_path())
            != Some(parent.provenance().join(&child).as_path())
    {
        return Err(fail(FailureKind::Conflict));
    }
    let anchor = Anchor {
        path: parent.provenance().join(&child),
        dir: directory,
        chain: header.recovery_chain.clone(),
        pinned: Some(expected),
    };
    Ok(Some(ReservedRecoveryV2 {
        parent: RecoveryParent::pin(&parent.directory, parent.expected, &parent.provenance)?,
        anchor,
        header,
    }))
}
impl Anchor {
    fn open(path: &Path) -> Result<Self> {
        if path.to_str().is_none()
            || !path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(fail(FailureKind::InvalidChangeSet));
        }
        let mut dir = File::open("/").map_err(io)?;
        let mut prefix = PathBuf::from("/");
        let mut chain = vec![(prefix.clone(), identity(&dir.metadata().map_err(io)?))];
        for component in path.components() {
            if let Component::Normal(name) = component {
                dir = open_at(&dir, name, true, false)?;
                prefix.push(name);
                chain.push((prefix.clone(), identity(&dir.metadata().map_err(io)?)));
            }
        }
        Ok(Self {
            path: path.into(),
            dir,
            chain,
            pinned: None,
        })
    }
    fn check(&self) -> Result<()> {
        if let Some(expected) = self.pinned {
            return private_directory(&self.dir, expected);
        }
        let now = Self::open(&self.path).map_err(|_| fail(FailureKind::AncestorChanged))?;
        if self.chain != now.chain {
            return Err(fail(FailureKind::AncestorChanged));
        }
        Ok(())
    }
}

fn exclusive(from: &File, name: &OsStr, to: &File, target: &OsStr) -> Result<()> {
    let name = cstr(name)?;
    let target = cstr(target)?;
    // EXCL tests destination absence; source identity is checked after capture.
    #[cfg(target_os = "macos")]
    let status = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if status != 0 {
            return Err(exclusive_error(std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(io(std::io::Error::from(std::io::ErrorKind::Unsupported)))
    }
}

fn exclusive_error(error: std::io::Error) -> ApplyFailure {
    let mut failure = io(error);
    if matches!(failure.os_error, Some(libc::EEXIST | libc::ENOENT)) {
        failure.kind = FailureKind::Conflict;
    }
    failure
}

fn matching(a: &ManifestEntry, b: &ManifestEntry) -> bool {
    a.relative_path == b.relative_path
        && a.identity == b.identity
        && a.bytes == b.bytes
        && a.mode == b.mode
        && a.sha256 == b.sha256
        && a.observed_mode() == b.observed_mode()
        && a.observed_mtime() == b.observed_mtime()
        && a.observed_ctime() == b.observed_ctime()
}
fn same_change(a: &Change, b: &Change) -> bool {
    let same = |a: &Option<ManifestEntry>, b: &Option<ManifestEntry>| match (a, b) {
        (Some(a), Some(b)) => matching(a, b),
        (None, None) => true,
        _ => false,
    };
    a.relative_path == b.relative_path
        && a.kind == b.kind
        && same(&a.before, &b.before)
        && same(&a.after, &b.after)
}

fn validate_changes(
    snapshot: &Snapshot,
    requested: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<ChangeSet> {
    // The requested public Vec and entries are not authoritative. A full fresh
    // change scan also detects omitted, duplicate, reordered and edited entries.
    let fresh = copy::collect_changes(snapshot, secrets, limits)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if !requested.is_applicable()
        || !fresh.is_applicable()
        || requested.changes.len() != fresh.changes.len()
        || !requested
            .changes
            .iter()
            .zip(&fresh.changes)
            .all(|(a, b)| same_change(a, b))
    {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    // Checks the request's private snapshot binding as well as the real source.
    let conflicts = copy::check_source_conflicts(snapshot, requested, secrets, limits)
        .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if !conflicts.is_empty() {
        return Err(fail(FailureKind::Conflict));
    }
    for change in &fresh.changes {
        if change.relative_path.to_str().is_none()
            || change
                .before
                .iter()
                .chain(change.after.iter())
                .any(|entry| entry.sha256.is_none())
        {
            return Err(fail(FailureKind::UnsupportedEntry));
        }
    }
    Ok(fresh)
}

/// Verify identity/registration before reading bytes, including a moved source.
fn verified_copy(
    file: &mut File,
    destination: Option<&mut File>,
    expected: &ManifestEntry,
    original_path: &Path,
    physical_path: &Path,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> Result<()> {
    let before = file.metadata().map_err(io)?;
    let secrets = copy::refreshed_secrets(secrets).map_err(|_| fail(FailureKind::Secret))?;
    if polaris_tools::path_policy::is_denied(original_path)
        || polaris_tools::path_policy::is_denied(physical_path)
        || secrets.denies(original_path, &before)
        || secrets.denies(physical_path, &before)
    {
        return Err(fail(FailureKind::Secret));
    }
    if !before.is_file() || before.nlink() != 1 {
        return Err(fail(FailureKind::UnsupportedEntry));
    }
    if identity(&before) != (expected.identity.device, expected.identity.inode)
        || before.len() != expected.bytes
        || before.mode() & 0o7777 != expected.observed_mode()
        || before.len() > limits.max_file_bytes
    {
        return Err(fail(FailureKind::Conflict));
    }
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    let mut destination = destination;
    loop {
        let n = file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or_else(|| fail(FailureKind::Conflict))?;
        if total > expected.bytes {
            return Err(fail(FailureKind::Conflict));
        }
        hash.update(&buffer[..n]);
        if let Some(output) = &mut destination {
            output.write_all(&buffer[..n]).map_err(io)?;
        }
    }
    if total != expected.bytes
        || Some(<[u8; 32]>::from(hash.finalize())) != expected.sha256
        || !stable(&before, &file.metadata().map_err(io)?)
    {
        return Err(fail(FailureKind::Conflict));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    BeforePrepare,
    AfterPrepared,
    BeforePreserve,
    AfterPreserve,
    AfterVerified,
    BeforeInstall,
    AfterInstall,
    BeforeRestore,
    AfterRestore,
    BeforeDelete,
    AfterDelete,
    BeforeJournal,
    AfterJournal,
}
type Hook<'a> = dyn FnMut(Point, usize, &Path) -> std::io::Result<()> + 'a;
fn checkpoint(hook: &mut Hook<'_>, point: Point, index: usize, recovery: &Path) -> Result<()> {
    #[cfg(test)]
    tests::panic_at_checkpoint(point);
    hook(point, index, recovery).map_err(io)
}

fn journal(
    file: &mut File,
    entry: &EntryReport,
    index: usize,
    recovery: &Path,
    hook: &mut Hook<'_>,
) -> Result<()> {
    checkpoint(hook, Point::BeforeJournal, index, recovery)?;
    serde_json::to_writer(&mut *file, entry).map_err(|_| fail(FailureKind::Io))?;
    file.write_all(b"\n").map_err(io)?;
    file.sync_all().map_err(io)?;
    checkpoint(hook, Point::AfterJournal, index, recovery)
}

/// `recovery_parent` must be parent-controlled, not exposed to any child, and on
/// the source filesystem. It must outlive snapshots; never place it in scratch.
/// Permission/generation authorization and recovery UI belong to the caller.
pub fn apply(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
) -> ApplyReport {
    apply_with_hook(
        snapshot,
        changes,
        secrets,
        limits,
        recovery_parent,
        &mut |_, _, _| Ok(()),
    )
}

/// Apply while the authentication owner's registry/rotation lock is held for
/// the entire operation, including candidate staging and source capture.
/// The caller must already have stopped snapshot writers and authorized the
/// current run/generation. This wrapper grants no execution or write authority.
/// Recovery location requirements are the same as `apply`. Do not call while
/// already holding the auth lock or from an auth registry callback.
pub fn protected_apply(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    limits: Limits,
    recovery_parent: &Path,
) -> ApplyReport {
    let protected = polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered).map_err(|_| ())?;
        Ok::<_, ()>(apply(snapshot, changes, &secrets, limits, recovery_parent))
    });
    match protected {
        Ok(Ok(report)) => report,
        _ => ApplyReport {
            source_path: snapshot.source_path.clone(),
            recovery_path: None,
            recovery_identity: None,
            recovery_directory: None,
            entries: vec![],
            failure: Some(fail(FailureKind::ProtectionUnavailable)),
        },
    }
}

/// Trusted FD-bound counterpart of `protected_apply`. The caller must stop copy
/// writers and durably authorize this exact parent identity before dispatch.
/// Do not call while holding the authentication registry lock.
pub fn protected_apply_pinned(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    limits: Limits,
    recovery_parent: &RecoveryParent,
) -> ApplyReport {
    let protected = polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = RegisteredSecrets::from_auth_registry(registered).map_err(|_| ())?;
        Ok::<_, ()>(apply_pinned_with_hook(
            snapshot,
            changes,
            &secrets,
            limits,
            recovery_parent,
            &mut |_, _, _| Ok(()),
        ))
    });
    protected
        .ok()
        .and_then(|report| report.ok())
        .unwrap_or_else(|| ApplyReport {
            source_path: snapshot.source_path.clone(),
            recovery_path: None,
            recovery_identity: None,
            recovery_directory: None,
            entries: vec![],
            failure: Some(fail(FailureKind::ProtectionUnavailable)),
        })
}

fn apply_pinned_with_hook(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &RecoveryParent,
    hook: &mut Hook<'_>,
) -> ApplyReport {
    apply_with_parent(
        snapshot,
        changes,
        secrets,
        limits,
        &recovery_parent.provenance,
        Some(recovery_parent),
        None,
        hook,
    )
}

fn apply_with_hook(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
    hook: &mut Hook<'_>,
) -> ApplyReport {
    apply_with_parent(
        snapshot,
        changes,
        secrets,
        limits,
        recovery_parent,
        None,
        None,
        hook,
    )
}

/// Dispatch only after the shared owner has durably started this exact effect.
/// It accepts a header-only reserved child; recovery of existing rows belongs
/// to the explicit joined recovery path, not to another apply attempt.
pub fn apply_reserved_v2(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    reserved: &ReservedRecoveryV2,
) -> ApplyReport {
    apply_with_parent(
        snapshot,
        changes,
        secrets,
        limits,
        reserved.parent.provenance(),
        Some(&reserved.parent),
        Some(reserved),
        &mut |_, _, _| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_with_parent(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
    pinned: Option<&RecoveryParent>,
    reserved: Option<&ReservedRecoveryV2>,
    hook: &mut Hook<'_>,
) -> ApplyReport {
    let mut report = ApplyReport {
        source_path: snapshot.source_path.clone(),
        recovery_path: None,
        recovery_identity: None,
        recovery_directory: None,
        entries: vec![],
        failure: None,
    };
    let outcome = run(
        snapshot,
        changes,
        secrets,
        limits,
        recovery_parent,
        pinned,
        reserved,
        hook,
        &mut report,
    );
    if let Err(error) = outcome {
        report.failure = Some(error);
    }
    report
}

#[allow(clippy::too_many_arguments)]
fn run(
    snapshot: &Snapshot,
    changes: &ChangeSet,
    secrets: &RegisteredSecrets,
    limits: Limits,
    recovery_parent: &Path,
    pinned: Option<&RecoveryParent>,
    reserved: Option<&ReservedRecoveryV2>,
    hook: &mut Hook<'_>,
    report: &mut ApplyReport,
) -> Result<()> {
    let source = Anchor::open(&snapshot.source_path)?;
    let owner = if let Some(pinned) = pinned {
        pinned.validate_identity()?;
        Anchor {
            path: pinned.provenance.clone(),
            dir: pinned.directory.try_clone().map_err(io)?,
            chain: vec![(pinned.provenance.clone(), pinned.expected)],
            pinned: Some(pinned.expected),
        }
    } else {
        Anchor::open(recovery_parent)?
    };
    if source.dir.metadata().map_err(io)?.dev() != owner.dir.metadata().map_err(io)?.dev() {
        return Err(fail(FailureKind::CrossDevice));
    }
    let snapshot_owner = Anchor::open(
        snapshot
            .path()
            .parent()
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?,
    )?;
    outside(&owner.dir, &source.dir)?;
    outside(&owner.dir, &snapshot_owner.dir)?;
    let fresh = validate_changes(snapshot, changes, secrets, limits)?;
    if fresh.changes.is_empty() && reserved.is_none() {
        return Ok(());
    }
    let (recovery, mut log) = if let Some(reserved) = reserved {
        if reserved.header.source != source.path
            || reserved.header.source_chain != source.chain
            || reserved.header.entries != journal_changes(&fresh.changes)?
        {
            return Err(fail(FailureKind::Conflict));
        }
        let mut expected = reserved.header.canonical_bytes()?;
        expected.push(b'\n');
        if reserved.inspect_journal(expected.len() as u64)? != expected {
            return Err(fail(FailureKind::Conflict));
        }
        require_header_only_entries(&reserved.anchor.dir)?;
        let recovery = Anchor {
            path: reserved.anchor.path.clone(),
            dir: reserved.anchor.dir.try_clone().map_err(io)?,
            chain: reserved.anchor.chain.clone(),
            pinned: reserved.anchor.pinned,
        };
        let name = cstr(OsStr::new("journal.jsonl"))?;
        let raw = unsafe {
            libc::openat(
                recovery.dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_APPEND
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        let mut log = unsafe { File::from_raw_fd(raw) };
        let before = log.metadata().map_err(io)?;
        if !before.is_file()
            || before.nlink() != 1
            || before.uid() != unsafe { libc::geteuid() }
            || before.mode() & 0o7777 != 0o600
            || before.len() != expected.len() as u64
        {
            return Err(fail(FailureKind::Conflict));
        }
        let mut reread = Vec::new();
        (&mut log)
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut reread)
            .map_err(io)?;
        let linked = open_at(&recovery.dir, OsStr::new("journal.jsonl"), false, false)?;
        if reread != expected
            || !stable(&before, &log.metadata().map_err(io)?)
            || !stable(&before, &linked.metadata().map_err(io)?)
        {
            return Err(fail(FailureKind::AncestorChanged));
        }
        (recovery, log)
    } else {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| fail(FailureKind::Io))?;
        let name = format!(
            "polaris-apply-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let recovery_path = recovery_parent.join(&name);
        owner.check()?;
        let c_name = cstr(OsStr::new(&name))?;
        if unsafe { libc::mkdirat(owner.dir.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        report.recovery_path = Some(recovery_path.clone());
        let directory = open_at(&owner.dir, OsStr::new(&name), true, false)?;
        let recovery_id = identity(&directory.metadata().map_err(io)?);
        let mut chain = owner.chain.clone();
        chain.push((recovery_path.clone(), recovery_id));
        let recovery = Anchor {
            path: recovery_path.clone(),
            dir: directory,
            chain,
            pinned: pinned.map(|_| recovery_id),
        };
        report.recovery_identity = Some(identity(&recovery.dir.metadata().map_err(io)?));
        report.recovery_directory = Some(recovery.dir.try_clone().map_err(io)?);
        owner.dir.sync_all().map_err(io)?;
        owner.check()?;
        recovery.check()?;
        let mut log = open_at(&recovery.dir, OsStr::new("journal.jsonl"), false, true)?;
        // Persist identities, baseline/target hashes and modes before any source move.
        let header = serde_json::json!({ "version": 1, "source": snapshot.source_path, "source_chain": source.chain,
            "recovery_chain": recovery.chain, "entries": fresh.changes.iter().map(|c| serde_json::json!({
                "path": c.relative_path, "before": c.before.as_ref().map(|e| serde_json::json!({"device": e.identity.device, "inode":e.identity.inode, "hash":e.sha256, "mode":e.observed_mode(), "bytes":e.bytes})),
                "after": c.after.as_ref().map(|e| serde_json::json!({"hash":e.sha256, "mode":e.mode, "bytes":e.bytes}))
            })).collect::<Vec<_>>() });
        serde_json::to_writer(&mut log, &header)
            .map_err(|_| fail(FailureKind::InvalidChangeSet))?;
        log.write_all(b"\n").map_err(io)?;
        log.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        (recovery, log)
    };
    let recovery_path = recovery.path.clone();
    report.recovery_path = Some(recovery_path.clone());
    report.recovery_identity = Some(identity(&recovery.dir.metadata().map_err(io)?));
    report.recovery_directory = Some(recovery.dir.try_clone().map_err(io)?);
    let mut parents = Vec::new();
    // Fix all candidates before opening a gap at any source entry.
    for (i, change) in fresh.changes.iter().enumerate() {
        let prepare = (|| {
            let parent_path = snapshot
                .source_path
                .join(&change.relative_path)
                .parent()
                .unwrap()
                .to_path_buf();
            let parent = Anchor::open(&parent_path)?;
            if parent.dir.metadata().map_err(io)?.dev()
                != recovery.dir.metadata().map_err(io)?.dev()
            {
                return Err(fail(FailureKind::CrossDevice));
            }
            let candidate_name = change.after.as_ref().map(|_| format!("candidate-{i}"));
            let install_name = change.after.as_ref().map(|_| format!("install-{i}"));
            report.entries.push(EntryReport {
                relative_path: change.relative_path.clone(),
                source_parent_chain: parent.chain.clone(),
                phase: Phase::Preparing,
                old_retained: false,
                installed: false,
                deleted: false,
                restored: false,
                old_name: format!("old-{i}"),
                candidate_name,
                install_name,
                candidate_identity: None,
                install_identity: None,
            });
            checkpoint(hook, Point::BeforePrepare, i, &recovery_path)?;
            if let Some(after) = &change.after {
                let copy_path = snapshot.path().join(&change.relative_path);
                let copy_parent = Anchor::open(copy_path.parent().unwrap())?;
                let mut input = open_at(
                    &copy_parent.dir,
                    copy_path.file_name().unwrap(),
                    false,
                    false,
                )?;
                let mut candidate = open_at(
                    &recovery.dir,
                    OsStr::new(report.entries[i].candidate_name.as_ref().unwrap()),
                    false,
                    true,
                )?;
                verified_copy(
                    &mut input,
                    Some(&mut candidate),
                    after,
                    &snapshot.source_path.join(&change.relative_path),
                    &copy_path,
                    secrets,
                    limits,
                )?;
                copy_parent.check()?;
                candidate.sync_all().map_err(io)?;
                report.entries[i].candidate_identity =
                    Some(identity(&candidate.metadata().map_err(io)?));
                candidate.seek(SeekFrom::Start(0)).map_err(io)?;
                let mut install = open_at(
                    &recovery.dir,
                    OsStr::new(report.entries[i].install_name.as_ref().unwrap()),
                    false,
                    true,
                )?;
                let copied = std::io::copy(&mut candidate, &mut install).map_err(io)?;
                if copied != after.bytes {
                    return Err(fail(FailureKind::Conflict));
                }
                install
                    .set_permissions(Permissions::from_mode(after.mode & 0o777))
                    .map_err(io)?;
                install.sync_all().map_err(io)?;
                report.entries[i].install_identity =
                    Some(identity(&install.metadata().map_err(io)?));
            }
            recovery.dir.sync_all().map_err(io)?;
            report.entries[i].phase = Phase::Prepared;
            journal(&mut log, &report.entries[i], i, &recovery_path, hook)?;
            checkpoint(hook, Point::AfterPrepared, i, &recovery_path)?;
            parents.push(parent);
            Ok(())
        })();
        prepare.map_err(|mut e: ApplyFailure| {
            e.entry = Some(i);
            e
        })?;
    }
    // Recheck after staging, before the first source mutation. Thereafter each
    // captured inode is validated independently; earlier successes are not undone.
    if !copy::check_source_conflicts(snapshot, &fresh, secrets, limits)
        .map_err(|_| fail(FailureKind::Conflict))?
        .is_empty()
    {
        return Err(fail(FailureKind::Conflict));
    }
    for (i, (change, parent)) in fresh.changes.iter().zip(&parents).enumerate() {
        let entry = &mut report.entries[i];
        let result = apply_one(
            change, parent, &recovery, &mut log, entry, i, snapshot, secrets, limits, hook,
        );
        result.map_err(|mut e| {
            e.entry = Some(i);
            e
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_one(
    change: &Change,
    parent: &Anchor,
    recovery: &Anchor,
    log: &mut File,
    entry: &mut EntryReport,
    i: usize,
    snapshot: &Snapshot,
    secrets: &RegisteredSecrets,
    limits: Limits,
    hook: &mut Hook<'_>,
) -> Result<()> {
    let name = change
        .relative_path
        .file_name()
        .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?;
    let check = || {
        parent.check()?;
        recovery.check()
    };
    check()?;
    if let Some(before) = &change.before {
        entry.phase = Phase::PreserveIntent;
        journal(log, entry, i, &recovery.path, hook)?;
        checkpoint(hook, Point::BeforePreserve, i, &recovery.path)?;
        check()?;
        exclusive(
            &parent.dir,
            name,
            &recovery.dir,
            OsStr::new(&entry.old_name),
        )?;
        entry.old_retained = true;
        checkpoint(hook, Point::AfterPreserve, i, &recovery.path)?;
        check()?;
        parent.dir.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        entry.phase = Phase::Preserved;
        journal(log, entry, i, &recovery.path, hook)?;
        let verification = (|| {
            let mut old = open_at(&recovery.dir, OsStr::new(&entry.old_name), false, false)?;
            verified_copy(
                &mut old,
                None,
                before,
                &snapshot.source_path.join(&change.relative_path),
                &recovery.path.join(&entry.old_name),
                secrets,
                limits,
            )?;
            old.sync_all().map_err(io)
        })();
        if let Err(original) = verification {
            restore_current(parent, recovery, log, entry, i, hook)?;
            return Err(original);
        }
        checkpoint(hook, Point::AfterVerified, i, &recovery.path)?;
    }
    if let Some(install) = &entry.install_name {
        entry.phase = Phase::InstallIntent;
        journal(log, entry, i, &recovery.path, hook)?;
        checkpoint(hook, Point::BeforeInstall, i, &recovery.path)?;
        check()?;
        if let Err(error) = exclusive(&recovery.dir, OsStr::new(install), &parent.dir, name) {
            // A known destination conflict can only restore into a now-empty
            // slot. Other I/O failures keep the intent unresolved for recovery.
            if error.os_error == Some(libc::EEXIST) && entry.old_retained {
                restore_current(parent, recovery, log, entry, i, hook)?;
            }
            return Err(error);
        }
        entry.installed = true;
        checkpoint(hook, Point::AfterInstall, i, &recovery.path)?;
        check()?;
        parent.dir.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        entry.phase = Phase::Installed;
    } else {
        entry.phase = Phase::DeleteIntent;
        journal(log, entry, i, &recovery.path, hook)?;
        checkpoint(hook, Point::BeforeDelete, i, &recovery.path)?;
        check()?;
        // Never unlink a writer's newly created entry while completing deletion.
        let c_name = cstr(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let status = unsafe {
            libc::fstatat(
                parent.dir.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status == 0 {
            return Err(fail(FailureKind::Conflict));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(io(error));
        }
        entry.deleted = true;
        checkpoint(hook, Point::AfterDelete, i, &recovery.path)?;
        check()?;
        parent.dir.sync_all().map_err(io)?;
        recovery.dir.sync_all().map_err(io)?;
        entry.phase = Phase::Deleted;
    }
    journal(log, entry, i, &recovery.path, hook)
}

fn restore_current(
    parent: &Anchor,
    recovery: &Anchor,
    log: &mut File,
    entry: &mut EntryReport,
    i: usize,
    hook: &mut Hook<'_>,
) -> Result<()> {
    parent.check()?;
    recovery.check()?;
    entry.phase = Phase::RestoreIntent;
    journal(log, entry, i, &recovery.path, hook)?;
    checkpoint(hook, Point::BeforeRestore, i, &recovery.path)?;
    parent.check()?;
    recovery.check()?;
    if let Err(mut error) = exclusive(
        &recovery.dir,
        OsStr::new(&entry.old_name),
        &parent.dir,
        entry
            .relative_path
            .file_name()
            .ok_or_else(|| fail(FailureKind::InvalidChangeSet))?,
    ) {
        error.kind = FailureKind::RestoreConflict;
        return Err(error);
    }
    entry.old_retained = false;
    entry.restored = true;
    checkpoint(hook, Point::AfterRestore, i, &recovery.path)?;
    parent.check()?;
    recovery.check()?;
    parent.dir.sync_all().map_err(io)?;
    recovery.dir.sync_all().map_err(io)?;
    entry.phase = Phase::Restored;
    journal(log, entry, i, &recovery.path, hook)
}

#[cfg(test)]
#[path = "workspace_apply/tests.rs"]
mod tests;
