//! Shared apply-report format; filesystem ownership stays in the native owner.
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Preparing,
    Prepared,
    PreserveIntent,
    Preserved,
    RestoreIntent,
    Restored,
    InstallIntent,
    Installed,
    DeleteIntent,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    ProtectionUnavailable,
    InvalidChangeSet,
    UnsupportedEntry,
    Conflict,
    Secret,
    AncestorChanged,
    CrossDevice,
    Io,
    RestoreConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyFailure {
    pub kind: FailureKind,
    pub entry: Option<usize>,
    pub os_error: Option<i32>,
}

/// Flags describe completed syscalls even if their subsequent sync/journal failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryReport {
    pub relative_path: PathBuf,
    /// Includes ancestors above source; a path may no longer name this directory.
    pub source_parent_chain: Vec<(PathBuf, (u64, u64))>,
    pub phase: Phase,
    pub old_retained: bool,
    pub installed: bool,
    pub deleted: bool,
    pub restored: bool,
    pub old_name: String,
    /// A separate fixed copy; installing never consumes this recovery version.
    pub candidate_name: Option<String>,
    pub install_name: Option<String>,
    pub candidate_identity: Option<(u64, u64)>,
    pub install_identity: Option<(u64, u64)>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyReport {
    pub source_path: PathBuf,
    /// Intended path, not a guarantee that an external rename left it attached.
    pub recovery_path: Option<PathBuf>,
    pub recovery_identity: Option<(u64, u64)>,
    #[serde(skip)]
    pub(in crate::workspace_apply) recovery_directory: Option<File>,
    pub entries: Vec<EntryReport>,
    pub failure: Option<ApplyFailure>,
}

impl ApplyReport {
    /// Remains usable if an ancestor of the recovery path was renamed. The
    /// caller may retain this handle while presenting/manual recovery proceeds.
    pub fn recovery_directory(&self) -> Option<&File> {
        self.recovery_directory.as_ref()
    }
}

pub const MAX_APPLY_REPORT_BYTES: u64 = 256 * 1024 * 1024;

impl ApplyReport {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| fail(FailureKind::InvalidChangeSet))?;
        bounded_evidence(&bytes, MAX_APPLY_REPORT_BYTES)?;
        Ok(bytes)
    }
}

pub fn decode_apply_report(bytes: &[u8]) -> Result<ApplyReport> {
    bounded_evidence(bytes, MAX_APPLY_REPORT_BYTES)?;
    decode_exact(bytes)
}

type Result<T> = std::result::Result<T, ApplyFailure>;
pub(in crate::workspace_apply) fn fail(kind: FailureKind) -> ApplyFailure {
    ApplyFailure {
        kind,
        entry: None,
        os_error: None,
    }
}

pub(in crate::workspace_apply) fn bounded_evidence(bytes: &[u8], maximum: u64) -> Result<()> {
    let length = u64::try_from(bytes.len()).map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if length > maximum {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    Ok(())
}

pub(in crate::workspace_apply) fn decode_exact<T: serde::de::DeserializeOwned + Serialize>(
    bytes: &[u8],
) -> Result<T> {
    let value: T =
        serde_json::from_slice(bytes).map_err(|_| fail(FailureKind::InvalidChangeSet))?;
    if serde_json::to_vec(&value).map_err(|_| fail(FailureKind::InvalidChangeSet))? != bytes {
        return Err(fail(FailureKind::InvalidChangeSet));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL: &[u8] = br#"{"source_path":"project","recovery_path":null,"recovery_identity":null,"entries":[],"failure":{"kind":"ProtectionUnavailable","entry":null,"os_error":null}}"#;

    #[test]
    fn report_round_trip_preserves_format_and_skips_the_live_handle() {
        let mut report = ApplyReport {
            source_path: PathBuf::from("project"),
            recovery_path: None,
            recovery_identity: None,
            recovery_directory: Some(tempfile::tempfile().unwrap()),
            entries: Vec::new(),
            failure: Some(fail(FailureKind::ProtectionUnavailable)),
        };
        assert!(report.recovery_directory().unwrap().metadata().is_ok());
        assert_eq!(report.canonical_bytes().unwrap(), CANONICAL);
        let decoded = decode_apply_report(CANONICAL).unwrap();
        assert_eq!(decoded.source_path, report.source_path);
        assert_eq!(decoded.failure, report.failure);
        assert!(decoded.recovery_directory().is_none());
        assert_eq!(decoded.canonical_bytes().unwrap(), CANONICAL);
        report.failure.as_mut().unwrap().kind = FailureKind::Conflict;
        assert_ne!(report.canonical_bytes().unwrap(), CANONICAL);
    }

    #[test]
    fn report_decode_rejects_noncanonical_and_unknown_fields() {
        let mut spaced = CANONICAL.to_vec();
        spaced.push(b'\n');
        let mut unknown = CANONICAL[..CANONICAL.len() - 1].to_vec();
        unknown.extend_from_slice(br#", "unexpected":true}"#);
        for bytes in [spaced.as_slice(), unknown.as_slice(), b"{}", b"null"] {
            assert_eq!(
                decode_apply_report(bytes).unwrap_err().kind,
                FailureKind::InvalidChangeSet
            );
        }
        assert_eq!(
            bounded_evidence(CANONICAL, CANONICAL.len() as u64 - 1)
                .unwrap_err()
                .kind,
            FailureKind::InvalidChangeSet
        );
    }
}
