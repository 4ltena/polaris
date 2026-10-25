//! Physical directory identity used by plan-bound host owners.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentity {
    pub canonical_path: String,
    pub device: u64,
    pub inode: u64,
}
