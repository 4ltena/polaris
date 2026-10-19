//! Durable, typed lifecycle records for management side effects.
//!
//! The journal deliberately stores only stable identifiers, hashes, numeric
//! revisions and closed enums.  Prompts, secrets, filesystem paths and free
//! form result text have no field in this contract.

#[cfg(unix)]
pub mod account_change;
#[cfg(unix)]
pub mod mcp_change;
#[cfg(unix)]
pub mod memory_change;
mod store;
#[cfg(all(test, unix))]
pub(crate) use store::generation::{
    ManagementMigrationAnchorV2, ManagementMigrationAnchorsV2, ManagementMigrationTreeV2,
};
pub mod v2;
#[cfg(unix)]
pub mod workflow_change;
#[cfg(unix)]
pub mod workspace_change;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};

#[cfg(unix)]
pub use store::JoinedManagementEffectRecoveryV1;
pub use store::{
    ManagementEffectCapacityV1, ManagementEffectInspectOpenV1, ManagementEffectInspectV1,
    ManagementEffectInspectedFileV1, ManagementEffectLimitsV1, ManagementEffectStoreError,
    ManagementEffectStoreV1, ManagementEffectTailV1,
};
#[cfg(unix)]
pub use workflow_change::{
    WorkflowChangeError, WorkflowChangeOwner, WorkflowChangePreviewV1, WorkflowEffectInventoryV1,
    WorkflowPreparedChangeV1,
};

const HASH_BYTES: usize = 32;
const ID_BYTES: usize = 16;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManagementOperationIdV1(HexIdV1<ID_BYTES>);

impl ManagementOperationIdV1 {
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; ID_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(HexIdV1(bytes)))
    }

    pub fn as_hex(&self) -> String {
        self.0.encode()
    }

    /// Stable child identity for a local session-switch effect. An unknown
    /// outcome can inspect this one target without replaying the mutation.
    pub fn child_session_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"polaris-management-child-v1");
        hasher.update(self.0.0);
        let digest = hasher.finalize();
        let mut bytes = [0_u8; ID_BYTES];
        bytes.copy_from_slice(&digest[..ID_BYTES]);
        bytes[6] = (bytes[6] & 15) | 64;
        bytes[8] = (bytes[8] & 63) | 128;
        let hex = HexIdV1(bytes).encode();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    }
}

impl fmt::Debug for ManagementOperationIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ManagementOperationIdV1")
            .field(&self.as_hex())
            .finish()
    }
}

impl fmt::Display for ManagementOperationIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

impl FromStr for ManagementOperationIdV1 {
    type Err = ManagementEffectValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(HexIdV1::parse(value)?))
    }
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManagementEffectHashV1(HexIdV1<HASH_BYTES>);

impl ManagementEffectHashV1 {
    pub fn digest(bytes: &[u8]) -> Self {
        let digest: [u8; HASH_BYTES] = Sha256::digest(bytes).into();
        Self(HexIdV1(digest))
    }

    pub fn from_hex(value: &str) -> Result<Self, ManagementEffectValueError> {
        Ok(Self(HexIdV1::parse(value)?))
    }

    pub fn as_hex(&self) -> String {
        self.0.encode()
    }

    /// Binds an effect to a saved provider registration without persisting its
    /// display name or credentials. Callers must verify the current registration.
    pub fn account_binding(
        provider_id: &str,
        registration_id: &str,
    ) -> Result<Self, ManagementEffectValueError> {
        let provider = provider_id.as_bytes();
        let registration = registration_id.as_bytes();
        if provider.is_empty()
            || !provider.iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
            })
            || registration.is_empty()
            || provider.len() > u16::MAX as usize
            || registration.len() > u16::MAX as usize
        {
            return Err(ManagementEffectValueError::InvalidAccountBinding);
        }
        let mut bytes = Vec::with_capacity(30 + provider.len() + registration.len());
        bytes.extend_from_slice(b"polaris-management-account-v1\0");
        bytes.extend_from_slice(&(provider.len() as u16).to_be_bytes());
        bytes.extend_from_slice(provider);
        bytes.extend_from_slice(&(registration.len() as u16).to_be_bytes());
        bytes.extend_from_slice(registration);
        Ok(Self::digest(&bytes))
    }

    fn zero() -> Self {
        Self(HexIdV1([0; HASH_BYTES]))
    }
}

impl fmt::Debug for ManagementEffectHashV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ManagementEffectHashV1")
            .field(&self.as_hex())
            .finish()
    }
}

impl fmt::Display for ManagementEffectHashV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

/// A row from the approved TUI parity inventory. No caller-defined label is
/// accepted, so this field cannot be repurposed to persist a prompt or secret.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManagementInventoryIdV1 {
    family: ManagementInventoryFamilyV1,
    number: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ManagementInventoryFamilyV1 {
    Cli,
    Desktop,
    Tui,
    Flag,
}

impl ManagementInventoryIdV1 {
    pub fn parse(value: &str) -> Result<Self, ManagementEffectValueError> {
        let bytes = value.as_bytes();
        if bytes.len() != 3 || !bytes[1].is_ascii_digit() || !bytes[2].is_ascii_digit() {
            return Err(ManagementEffectValueError::InvalidInventoryId);
        }
        let number = (bytes[1] - b'0') * 10 + (bytes[2] - b'0');
        let (family, maximum) = match bytes[0] {
            b'C' => (ManagementInventoryFamilyV1::Cli, 42),
            b'D' => (ManagementInventoryFamilyV1::Desktop, 41),
            b'T' => (ManagementInventoryFamilyV1::Tui, 21),
            b'F' => (ManagementInventoryFamilyV1::Flag, 25),
            _ => return Err(ManagementEffectValueError::InvalidInventoryId),
        };
        if number == 0 || number > maximum {
            return Err(ManagementEffectValueError::InvalidInventoryId);
        }
        Ok(Self { family, number })
    }

    pub fn encoded(self) -> String {
        let family = match self.family {
            ManagementInventoryFamilyV1::Cli => 'C',
            ManagementInventoryFamilyV1::Desktop => 'D',
            ManagementInventoryFamilyV1::Tui => 'T',
            ManagementInventoryFamilyV1::Flag => 'F',
        };
        format!("{family}{:02}", self.number)
    }
}

impl fmt::Display for ManagementInventoryIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.encoded())
    }
}

impl Serialize for ManagementInventoryIdV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.encoded())
    }
}

impl<'de> Deserialize<'de> for ManagementInventoryIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementPermissionScopeV1 {
    LocalSession,
    Project,
    Clipboard,
    System,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementRewindDispositionV1 {
    CapturedRestorable,
    IrreversibleLocal,
    IrreversibleRemote,
    RuntimeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementEffectScopeV1 {
    pub project: ManagementEffectHashV1,
    pub session: Option<ManagementEffectHashV1>,
    pub generation: u64,
    pub revision: u64,
    pub permission: ManagementPermissionScopeV1,
    pub account_binding: Option<ManagementEffectHashV1>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareManagementEffectV1 {
    pub operation_id: ManagementOperationIdV1,
    pub inventory: ManagementInventoryIdV1,
    pub scope: ManagementEffectScopeV1,
    pub request_hash: ManagementEffectHashV1,
    pub rewind: ManagementRewindDispositionV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementDenialReasonV1 {
    UserDeclined,
    PolicyDenied,
    PermissionDenied,
    StaleRequest,
    ResourceUnavailable,
    InvalidState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementUnknownReasonV1 {
    ProcessRestart,
    InterruptedAfterStart,
    ReceiptPersistenceFailed,
    DetachedEffect,
    UnclassifiedEffect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementEffectStateV1 {
    Prepared,
    EffectStarted,
    OutcomeUnknown {
        reason: ManagementUnknownReasonV1,
    },
    Completed {
        evidence_hash: ManagementEffectHashV1,
    },
    Denied {
        reason: ManagementDenialReasonV1,
    },
    ReconciledAbandon {
        evidence_hash: ManagementEffectHashV1,
    },
}

impl ManagementEffectStateV1 {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Denied { .. } | Self::ReconciledAbandon { .. }
        )
    }

    pub fn blocks_new_work(&self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementEffectSnapshotV1 {
    pub request: PrepareManagementEffectV1,
    pub state: ManagementEffectStateV1,
    pub last_sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementEffectCursorV1 {
    generation: u64,
    chain_hash: ManagementEffectHashV1,
    sequence: u64,
    journal_hash: ManagementEffectHashV1,
}

impl ManagementEffectCursorV1 {
    pub fn generation(self) -> u64 {
        self.generation
    }
}

/// Verified, durable management prefix at a quiescent workspace boundary.
/// Absence in legacy rewind metadata is never this typed empty baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementRewindCheckpointV1 {
    version: u8,
    project: ManagementEffectHashV1,
    cursor: ManagementEffectCursorV1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagementRewindCrossingV1 {
    Clear,
    Blocked {
        operation_id: ManagementOperationIdV1,
        inventory: ManagementInventoryIdV1,
        rewind: ManagementRewindDispositionV1,
        state: ManagementEffectStateV1,
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareManagementEffectOutcomeV1 {
    Prepared(PreparedManagementEffectV1),
    Existing(ManagementEffectSnapshotV1),
}

/// The original journal position, independent of the current store cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagementEffectEventEvidenceV1 {
    pub generation: u64,
    pub sequence: u64,
    pub event_hash: ManagementEffectHashV1,
}

/// Shares the existing journal wire format; snapshots and checkpoints stay v1.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementEffectEventV1 {
    Prepared {
        request: PrepareManagementEffectV1,
    },
    EffectStarted {
        operation_id: ManagementOperationIdV1,
    },
    OutcomeUnknown {
        operation_id: ManagementOperationIdV1,
        reason: ManagementUnknownReasonV1,
    },
    Completed {
        operation_id: ManagementOperationIdV1,
        evidence_hash: ManagementEffectHashV1,
    },
    Denied {
        operation_id: ManagementOperationIdV1,
        reason: ManagementDenialReasonV1,
    },
    ReconciledAbandon {
        operation_id: ManagementOperationIdV1,
        evidence_hash: ManagementEffectHashV1,
    },
}

impl ManagementEffectEventV1 {
    pub fn operation_id(&self) -> ManagementOperationIdV1 {
        match self {
            Self::Prepared { request } => request.operation_id,
            Self::EffectStarted { operation_id }
            | Self::OutcomeUnknown { operation_id, .. }
            | Self::Completed { operation_id, .. }
            | Self::Denied { operation_id, .. }
            | Self::ReconciledAbandon { operation_id, .. } => *operation_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedManagementEffectEventV1 {
    pub evidence: ManagementEffectEventEvidenceV1,
    pub event: ManagementEffectEventV1,
}

macro_rules! typed_effect {
    ($name:ident) => {
        typed_effect!($name, VerifiedManagementEffectEventV1);
    };
    ($name:ident, $event:ty) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name {
            verified_event: $event,
            request: PrepareManagementEffectV1,
            generation: u64,
            chain_hash: ManagementEffectHashV1,
            last_sequence: u64,
        }

        impl $name {
            pub fn verified_event(&self) -> &VerifiedManagementEffectEventV1 {
                &self.verified_event
            }

            pub fn operation_id(&self) -> ManagementOperationIdV1 {
                self.request.operation_id
            }

            pub fn request(&self) -> &PrepareManagementEffectV1 {
                &self.request
            }

            pub fn last_sequence(&self) -> u64 {
                self.last_sequence
            }

            pub fn generation(&self) -> u64 {
                self.generation
            }
        }
    };
}

typed_effect!(
    PreparedManagementEffectV1,
    Box<VerifiedManagementEffectEventV1>
);
typed_effect!(StartedManagementEffectV1);
typed_effect!(UnknownManagementEffectV1);
typed_effect!(CompletedManagementEffectV1);
typed_effect!(DeniedManagementEffectV1);
typed_effect!(AbandonedManagementEffectV1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagementEffectValueError {
    InvalidHex,
    InvalidInventoryId,
    InvalidAccountBinding,
}

impl fmt::Display for ManagementEffectValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHex => formatter.write_str("invalid lowercase hexadecimal value"),
            Self::InvalidInventoryId => formatter.write_str("invalid management inventory id"),
            Self::InvalidAccountBinding => {
                formatter.write_str("invalid management account binding")
            }
        }
    }
}

impl std::error::Error for ManagementEffectValueError {}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HexIdV1<const N: usize>([u8; N]);

impl<const N: usize> HexIdV1<N> {
    fn parse(value: &str) -> Result<Self, ManagementEffectValueError> {
        if value.len() != N * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ManagementEffectValueError::InvalidHex);
        }
        let mut bytes = [0_u8; N];
        for (index, output) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *output = u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| ManagementEffectValueError::InvalidHex)?;
        }
        Ok(Self(bytes))
    }

    fn encode(&self) -> String {
        let mut output = String::with_capacity(N * 2);
        for byte in self.0 {
            use std::fmt::Write as _;
            write!(output, "{byte:02x}").expect("String writes cannot fail");
        }
        output
    }
}

impl<const N: usize> Serialize for HexIdV1<N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.encode())
    }
}

impl<'de, const N: usize> Deserialize<'de> for HexIdV1<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
mod pure_tests {
    use super::*;

    fn operation_id(value: u8) -> ManagementOperationIdV1 {
        ManagementOperationIdV1::from_str(&format!("{value:032x}")).unwrap()
    }

    #[test]
    fn operation_id_has_one_stable_child_session_identity() {
        let first = operation_id(1);
        let second = operation_id(2);
        let child = first.child_session_id();
        assert_eq!(child, first.child_session_id());
        assert_ne!(child, second.child_session_id());
        assert_eq!(child.len(), 36);
        assert_eq!(&child[14..15], "4");
        assert!(matches!(&child[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn account_binding_hash_is_stable_and_unambiguous() {
        assert_eq!(
            ManagementEffectHashV1::account_binding("openai", "registration-42")
                .unwrap()
                .as_hex(),
            "fc791fdd44ddeb732e88ddb613e67ed7295bf64b593872ba406bf3752544bed7"
        );
        assert_ne!(
            ManagementEffectHashV1::account_binding("ab", "c").unwrap(),
            ManagementEffectHashV1::account_binding("a", "bc").unwrap()
        );
        assert!(ManagementEffectHashV1::account_binding("OpenAI", "registration-42").is_err());
        assert!(ManagementEffectHashV1::account_binding("openai", "").is_err());
    }

    #[test]
    fn management_model_hash_and_inventory_parse_boundaries() {
        assert!(ManagementEffectHashV1::from_hex("AA").is_err());
        assert!(ManagementInventoryIdV1::parse("../../secret").is_err());
        assert!(ManagementInventoryIdV1::parse("é1").is_err());
        assert!(ManagementInventoryIdV1::parse("T22").is_err());
        assert!(ManagementInventoryIdV1::parse("C42").is_ok());
        assert!(ManagementInventoryIdV1::parse("D41").is_ok());
        assert!(ManagementInventoryIdV1::parse("F25").is_ok());
    }
}
