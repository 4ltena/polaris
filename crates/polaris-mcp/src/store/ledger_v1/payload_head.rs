//! Pure canonical head bytes shared by migration observations and ledger tests.

#[cfg(test)]
use super::CredentialLedgerSnapshotV1;
use super::digest;
use crate::{Error, registration::bounded_json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// Includes the largest canonical numeric fields and both 64-byte hashes.
pub(super) const MAX_HEAD_BYTES: usize = 1024;

/// The initial head deliberately contains no epoch-file hash: it is created
/// before final epoch bytes and belongs to the activation payload projection.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerHeadV1 {
    pub(super) schema_version: u32,
    pub(super) ledger_epoch_id: String,
    pub(super) sequence: u64,
    pub(super) last_record_hash: Option<String>,
    pub(super) history_bytes: u64,
    pub(super) history_hash: String,
}
impl LedgerHeadV1 {
    fn empty(epoch_id: &str) -> Result<Self, Error> {
        if !digest(epoch_id) {
            return Err(Error::Protocol);
        }
        Ok(Self {
            schema_version: 1,
            ledger_epoch_id: epoch_id.to_owned(),
            sequence: 0,
            last_record_hash: None,
            history_bytes: 0,
            history_hash: bytes_hash(&[]),
        })
    }
    #[cfg(test)]
    pub(super) fn current(snapshot: &CredentialLedgerSnapshotV1, history: &[u8]) -> Self {
        Self {
            schema_version: 1,
            ledger_epoch_id: snapshot.epoch.random_epoch_id.clone(),
            sequence: snapshot.sequence,
            last_record_hash: (snapshot.sequence != 0).then(|| snapshot.head_hash.clone()),
            history_bytes: history.len() as u64,
            history_hash: bytes_hash(history),
        }
    }
    pub(super) fn encode(&self) -> Result<Vec<u8>, Error> {
        bounded_json(self, MAX_HEAD_BYTES)
    }
    #[cfg(test)]
    pub(super) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > MAX_HEAD_BYTES {
            return Err(Error::Limit);
        }
        let head: Self = serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?;
        if head.encode()? != bytes {
            return Err(Error::Protocol);
        }
        Ok(head)
    }
}

/// Pure pre-epoch payload bytes; this grants no filesystem write or migration
/// capability. The physical owner must independently validate and fsync them.
pub fn empty_payload_head(epoch_id: &str) -> Result<Vec<u8>, Error> {
    LedgerHeadV1::empty(epoch_id)?.encode()
}

pub(super) fn bytes_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
