//! The registration a run was bound to when it was accepted. Evidence only:
//! the credential is still resolved by the owner through this identity, and a
//! record without one predates multiple accounts and stays on the single
//! authentication.
use super::{StoreError, StoreResult};
use polaris_desktop_protocol::ids::DecimalU64;
use serde::{Deserialize, Serialize};

const MAX_REGISTRATION_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedAccount {
    pub registration_id: String,
    pub generation: DecimalU64,
}

impl SavedAccount {
    pub fn new(registration_id: String, generation: u64) -> StoreResult<Self> {
        let saved = Self {
            registration_id,
            generation: DecimalU64::new(generation),
        };
        saved.validate()?;
        Ok(saved)
    }

    /// Call after deserialization and before publication.
    pub fn validate(&self) -> StoreResult<()> {
        if self.generation.get() == 0
            || self.registration_id.is_empty()
            || self.registration_id.len() > MAX_REGISTRATION_BYTES
            || !self
                .registration_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(StoreError::Corrupt("run account"));
        }
        Ok(())
    }
}
