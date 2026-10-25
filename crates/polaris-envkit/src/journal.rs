//! Small append-only receipts inside one retained private operation directory.
//! Recovery reads progress; it never downloads, executes or republishes.
use crate::{Error, Stage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub sequence: u32,
    pub plan_sha256: String,
    pub previous_sha256: String,
    pub stage: Stage,
    pub candidate: usize,
    pub inventory_sha256: Option<String>,
    pub compatibility_failure: Option<crate::Failure>,
}

pub struct Journal {
    file: File,
    progress: Progress,
    last_sha256: String,
    bytes: u64,
    poisoned: bool,
}

impl Journal {
    /// The project management owner holds its lease across this journal's
    /// lifetime. A pre-existing journal is never overwritten or reexecuted.
    #[cfg(unix)]
    pub fn create(root: &Path, plan_sha256: &str, first_candidate: usize) -> Result<Self, Error> {
        use std::os::unix::fs::OpenOptionsExt;
        if !crate::is_digest(plan_sha256) {
            return Err(Error::StalePlan);
        }
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_APPEND)
            .mode(0o600)
            .open(root.join("progress.jsonl"))
            .map_err(|_| Error::RecoveryRequired)?;
        let progress = Progress {
            sequence: 0,
            plan_sha256: plan_sha256.into(),
            previous_sha256: "0".repeat(64),
            stage: Stage::Prepared,
            candidate: first_candidate,
            inventory_sha256: None,
            compatibility_failure: None,
        };
        let mut journal = Self {
            file,
            progress: progress.clone(),
            last_sha256: "0".repeat(64),
            bytes: 0,
            poisoned: false,
        };
        journal.append(progress)?;
        File::open(root)
            .and_then(|file| file.sync_all())
            .map_err(|_| Error::RecoveryRequired)?;
        Ok(journal)
    }

    /// A torn or malformed journal needs explicit reconciliation; it is not
    /// silently truncated, accepted as verified, or treated as a fresh run.
    #[cfg(unix)]
    pub fn open(root: &Path, plan_sha256: &str) -> Result<Self, Error> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join("progress.jsonl"))
            .map_err(|_| Error::RecoveryRequired)?;
        let metadata = file.metadata().map_err(|_| Error::RecoveryRequired)?;
        if !metadata.is_file()
            || metadata.len() > MAX_BYTES
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(Error::RecoveryRequired);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::RecoveryRequired)?;
        if bytes.len() as u64 > MAX_BYTES || !bytes.ends_with(b"\n") {
            return Err(Error::RecoveryRequired);
        }
        let mut last: Option<Progress> = None;
        let mut hash = "0".repeat(64);
        for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
            let progress: Progress =
                serde_json::from_slice(line).map_err(|_| Error::RecoveryRequired)?;
            if progress.plan_sha256 != plan_sha256
                || progress.previous_sha256 != hash
                || !crate::is_digest(&progress.plan_sha256)
                || progress
                    .inventory_sha256
                    .as_ref()
                    .is_some_and(|value| !crate::is_digest(value))
            {
                return Err(Error::RecoveryRequired);
            }
            if matches!(
                progress.stage,
                Stage::Staged | Stage::PublishIntent | Stage::Published | Stage::Verified
            ) && progress.inventory_sha256.is_none()
            {
                return Err(Error::RecoveryRequired);
            }
            if let Some(previous) = &last {
                if progress.sequence != previous.sequence + 1 {
                    return Err(Error::RecoveryRequired);
                }
                if progress.stage == Stage::Started
                    && previous.stage == Stage::Started
                    && progress.candidate > previous.candidate
                    && matches!(
                        progress.compatibility_failure,
                        Some(
                            crate::Failure::RuntimeIncompatible
                                | crate::Failure::PrimaryIncompatible
                        )
                    )
                {
                    if progress.inventory_sha256.is_some() {
                        return Err(Error::RecoveryRequired);
                    }
                } else {
                    previous.stage.advance(progress.stage)?;
                    if progress.candidate != previous.candidate
                        || progress.compatibility_failure.is_some()
                        || (previous.inventory_sha256.is_some()
                            && previous.inventory_sha256 != progress.inventory_sha256)
                    {
                        return Err(Error::RecoveryRequired);
                    }
                }
            } else if progress.sequence != 0 || progress.stage != Stage::Prepared {
                return Err(Error::RecoveryRequired);
            }
            hash = format!("{:x}", Sha256::digest(line));
            last = Some(progress);
        }
        Ok(Self {
            file,
            progress: last.ok_or(Error::RecoveryRequired)?,
            last_sha256: hash,
            bytes: bytes.len() as u64,
            poisoned: false,
        })
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn advance(&mut self, stage: Stage, inventory: Option<String>) -> Result<(), Error> {
        self.progress.stage.advance(stage)?;
        if matches!(
            stage,
            Stage::Staged | Stage::PublishIntent | Stage::Published | Stage::Verified
        ) && inventory
            .as_ref()
            .is_none_or(|hash| !crate::is_digest(hash))
        {
            return Err(Error::RecoveryRequired);
        }
        if self.progress.inventory_sha256.is_some() && self.progress.inventory_sha256 != inventory {
            return Err(Error::RecoveryRequired);
        }
        self.append(Progress {
            sequence: self
                .progress
                .sequence
                .checked_add(1)
                .ok_or(Error::RecoveryRequired)?,
            plan_sha256: self.progress.plan_sha256.clone(),
            previous_sha256: self.last_sha256.clone(),
            stage,
            candidate: self.progress.candidate,
            inventory_sha256: inventory,
            compatibility_failure: None,
        })
    }

    fn append(&mut self, progress: Progress) -> Result<(), Error> {
        if self.poisoned
            || self
                .file
                .metadata()
                .map_err(|_| Error::RecoveryRequired)?
                .len()
                != self.bytes
        {
            return Err(Error::RecoveryRequired);
        }
        let mut line = serde_json::to_vec(&progress).map_err(|_| Error::RecoveryRequired)?;
        let hash = format!("{:x}", Sha256::digest(&line));
        line.push(b'\n');
        let bytes = self
            .bytes
            .checked_add(line.len() as u64)
            .ok_or(Error::RecoveryRequired)?;
        if bytes > MAX_BYTES {
            return Err(Error::RecoveryRequired);
        }
        self.poisoned = true;
        self.file
            .write_all(&line)
            .and_then(|_| self.file.sync_all())
            .map_err(|_| Error::RecoveryRequired)?;
        self.progress = progress;
        self.last_sha256 = hash;
        self.bytes = bytes;
        self.poisoned = false;
        Ok(())
    }

    pub(crate) fn record_schema2_fallback(
        &mut self,
        next: usize,
        failure: crate::Failure,
    ) -> Result<(), Error> {
        if self.progress.stage != Stage::Started
            || next <= self.progress.candidate
            || !matches!(
                failure,
                crate::Failure::RuntimeIncompatible | crate::Failure::PrimaryIncompatible
            )
        {
            return Err(Error::StalePlan);
        }
        self.append(Progress {
            sequence: self
                .progress
                .sequence
                .checked_add(1)
                .ok_or(Error::RecoveryRequired)?,
            plan_sha256: self.progress.plan_sha256.clone(),
            previous_sha256: self.last_sha256.clone(),
            stage: Stage::Started,
            candidate: next,
            inventory_sha256: None,
            compatibility_failure: Some(failure),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn recovered_started_run_remains_started_and_does_not_execute() {
        let root = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let mut journal = Journal::create(root.path(), &hash, 0).unwrap();
        journal.advance(Stage::Started, None).unwrap();
        drop(journal);
        let mut recovered = Journal::open(root.path(), &hash).unwrap();
        assert_eq!(recovered.progress().stage, Stage::Started);
        assert_eq!(
            recovered.advance(Stage::Started, None),
            Err(Error::RecoveryRequired)
        );
        assert!(Journal::create(root.path(), &hash, 0).is_err());
        assert!(Journal::open(root.path(), &"b".repeat(64)).is_err());
    }

    #[test]
    fn torn_receipt_and_skipped_publish_are_not_success() {
        let root = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let mut journal = Journal::create(root.path(), &hash, 0).unwrap();
        assert_eq!(
            journal.advance(Stage::Published, Some(hash.clone())),
            Err(Error::RecoveryRequired)
        );
        drop(journal);
        OpenOptions::new()
            .append(true)
            .open(root.path().join("progress.jsonl"))
            .unwrap()
            .write_all(b"{\"sequence\":")
            .unwrap();
        assert!(Journal::open(root.path(), &hash).is_err());
    }

    #[test]
    fn inventory_is_fixed_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let mut journal = Journal::create(root.path(), &hash, 0).unwrap();
        journal.advance(Stage::Started, None).unwrap();
        journal.advance(Stage::Staged, Some(hash.clone())).unwrap();
        assert_eq!(
            journal.advance(Stage::PublishIntent, Some("b".repeat(64))),
            Err(Error::RecoveryRequired)
        );
        journal
            .advance(Stage::PublishIntent, Some(hash.clone()))
            .unwrap();
        journal
            .advance(Stage::Published, Some(hash.clone()))
            .unwrap();
        journal
            .advance(Stage::Verified, Some(hash.clone()))
            .unwrap();
        drop(journal);
        assert_eq!(
            Journal::open(root.path(), &hash).unwrap().progress().stage,
            Stage::Verified
        );
    }
}
