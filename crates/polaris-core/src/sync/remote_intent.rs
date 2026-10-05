//! Durable request and receipt types for one state-changing sync remote call.
//!
//! The request contains every identity needed to prove what was intended.
//! In particular, an authenticated account binding is mandatory and is never
//! inferred from Git configuration or another ambient credential source.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};

const DIGEST_BYTES: usize = 32;

macro_rules! opaque_id {
    ($name:ident, $invalid:ident, $max:expr) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, SyncRemoteValueErrorV1> {
                let value = value.into();
                if value.is_empty()
                    || value.len() > $max
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(byte, b'.' | b':' | b'_' | b'-' | b'/')
                    })
                {
                    return Err(SyncRemoteValueErrorV1::$invalid);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = SyncRemoteValueErrorV1;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(SyncRemoteOperationIdV1, InvalidOperationId, 96);
opaque_id!(SyncRemoteIdempotencyKeyV1, InvalidIdempotencyKey, 128);
opaque_id!(SyncShareIdV1, InvalidShareId, 128);
opaque_id!(SyncAccountProviderV1, InvalidAccountProvider, 64);
opaque_id!(
    SyncAccountRegistrationIdV1,
    InvalidAccountRegistrationId,
    128
);
opaque_id!(SyncRemoteReceiptIdV1, InvalidReceiptId, 128);

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SyncDigestV1([u8; DIGEST_BYTES]);

impl SyncDigestV1 {
    pub fn digest(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn from_hex(value: &str) -> Result<Self, SyncRemoteValueErrorV1> {
        if value.len() != DIGEST_BYTES * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SyncRemoteValueErrorV1::InvalidDigest);
        }
        let mut bytes = [0_u8; DIGEST_BYTES];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| SyncRemoteValueErrorV1::InvalidDigest)?;
        }
        Ok(Self(bytes))
    }

    pub fn as_hex(&self) -> String {
        let mut output = String::with_capacity(DIGEST_BYTES * 2);
        for byte in self.0 {
            use std::fmt::Write as _;
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
        }
        output
    }
}

impl fmt::Debug for SyncDigestV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SyncDigestV1")
            .field(&self.as_hex())
            .finish()
    }
}

impl fmt::Display for SyncDigestV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

impl FromStr for SyncDigestV1 {
    type Err = SyncRemoteValueErrorV1;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_hex(value)
    }
}

impl Serialize for SyncDigestV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_hex())
    }
}

impl<'de> Deserialize<'de> for SyncDigestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_hex(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CanonicalSyncRemoteV1(String);

impl CanonicalSyncRemoteV1 {
    /// Accepts a remote only after the caller has resolved aliases and
    /// configuration to the exact endpoint that will receive the operation.
    pub fn from_canonical(value: impl Into<String>) -> Result<Self, SyncRemoteValueErrorV1> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 2_048
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(SyncRemoteValueErrorV1::InvalidCanonicalRemote);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CanonicalSyncRemoteV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CanonicalSyncRemoteV1")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for CanonicalSyncRemoteV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CanonicalSyncRemoteV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_canonical(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SyncRemoteRefV1(String);

impl SyncRemoteRefV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, SyncRemoteValueErrorV1> {
        let value = value.into();
        let valid = value.starts_with("refs/")
            && value.len() <= 1_024
            && !value.ends_with('/')
            && !value.ends_with('.')
            && !value.contains("..")
            && !value.contains("@{")
            && !value.contains("//")
            && !value
                .bytes()
                .any(|byte| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'))
            && !value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            && value
                .split('/')
                .all(|component| !component.starts_with('.') && !component.ends_with(".lock"));
        if !valid {
            return Err(SyncRemoteValueErrorV1::InvalidRemoteRef);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SyncRemoteRefV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SyncRemoteRefV1")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for SyncRemoteRefV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SyncRemoteRefV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SyncGitObjectIdV1(String);

impl SyncGitObjectIdV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, SyncRemoteValueErrorV1> {
        let value = value.into();
        if !matches!(value.len(), 40 | 64)
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SyncRemoteValueErrorV1::InvalidGitObjectId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SyncGitObjectIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SyncGitObjectIdV1")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for SyncGitObjectIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SyncGitObjectIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "expectation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectedSyncRemoteOidV1 {
    Missing,
    Present { oid: SyncGitObjectIdV1 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncAccountBindingV1 {
    pub provider: SyncAccountProviderV1,
    pub registration_id: SyncAccountRegistrationIdV1,
    /// A digest of the stable provider subject, not a token or display label.
    pub subject_hash: SyncDigestV1,
}

impl SyncAccountBindingV1 {
    pub fn new(
        provider: SyncAccountProviderV1,
        registration_id: SyncAccountRegistrationIdV1,
        subject_hash: SyncDigestV1,
    ) -> Self {
        Self {
            provider,
            registration_id,
            subject_hash,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoteIntentRequestV1 {
    pub operation_id: SyncRemoteOperationIdV1,
    pub idempotency_key: SyncRemoteIdempotencyKeyV1,
    pub project: SyncDigestV1,
    pub share_id: SyncShareIdV1,
    pub canonical_remote: CanonicalSyncRemoteV1,
    pub remote_ref: SyncRemoteRefV1,
    pub local_commit: SyncGitObjectIdV1,
    pub expected_remote_oid: ExpectedSyncRemoteOidV1,
    pub account_binding: SyncAccountBindingV1,
    pub permission_hash: SyncDigestV1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentSyncRemoteStateV1 {
    pub project: SyncDigestV1,
    pub share_id: SyncShareIdV1,
    pub canonical_remote: CanonicalSyncRemoteV1,
    pub remote_ref: SyncRemoteRefV1,
    pub local_commit: SyncGitObjectIdV1,
    pub expected_remote_oid: ExpectedSyncRemoteOidV1,
    pub account_binding: SyncAccountBindingV1,
    pub permission_hash: SyncDigestV1,
}

impl CurrentSyncRemoteStateV1 {
    /// The mandatory typed binding prevents callers from exposing push while
    /// no authenticated account has been selected explicitly.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project: SyncDigestV1,
        share_id: SyncShareIdV1,
        canonical_remote: CanonicalSyncRemoteV1,
        remote_ref: SyncRemoteRefV1,
        local_commit: SyncGitObjectIdV1,
        expected_remote_oid: ExpectedSyncRemoteOidV1,
        account_binding: SyncAccountBindingV1,
        permission_hash: SyncDigestV1,
    ) -> Self {
        Self {
            project,
            share_id,
            canonical_remote,
            remote_ref,
            local_commit,
            expected_remote_oid,
            account_binding,
            permission_hash,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRemoteReceiptSourceV1 {
    TransportAcknowledged { receipt_id: SyncRemoteReceiptIdV1 },
    ReconciledExactRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoteReceiptV1 {
    pub operation_id: SyncRemoteOperationIdV1,
    pub idempotency_key: SyncRemoteIdempotencyKeyV1,
    pub account_binding: SyncAccountBindingV1,
    pub canonical_remote: CanonicalSyncRemoteV1,
    pub remote_ref: SyncRemoteRefV1,
    pub remote_oid: SyncGitObjectIdV1,
    pub source: SyncRemoteReceiptSourceV1,
}

impl SyncRemoteReceiptV1 {
    pub fn transport_acknowledged(
        request: &SyncRemoteIntentRequestV1,
        receipt_id: SyncRemoteReceiptIdV1,
        remote_oid: SyncGitObjectIdV1,
    ) -> Self {
        Self::for_request(
            request,
            remote_oid,
            SyncRemoteReceiptSourceV1::TransportAcknowledged { receipt_id },
        )
    }

    pub(crate) fn reconciled_exact_ref(request: &SyncRemoteIntentRequestV1) -> Self {
        Self::for_request(
            request,
            request.local_commit.clone(),
            SyncRemoteReceiptSourceV1::ReconciledExactRef,
        )
    }

    fn for_request(
        request: &SyncRemoteIntentRequestV1,
        remote_oid: SyncGitObjectIdV1,
        source: SyncRemoteReceiptSourceV1,
    ) -> Self {
        Self {
            operation_id: request.operation_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            account_binding: request.account_binding.clone(),
            canonical_remote: request.canonical_remote.clone(),
            remote_ref: request.remote_ref.clone(),
            remote_oid,
            source,
        }
    }

    pub(crate) fn proves(&self, request: &SyncRemoteIntentRequestV1) -> bool {
        self.operation_id == request.operation_id
            && self.idempotency_key == request.idempotency_key
            && self.account_binding == request.account_binding
            && self.canonical_remote == request.canonical_remote
            && self.remote_ref == request.remote_ref
            && self.remote_oid == request.local_commit
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRemoteUnknownReasonV1 {
    TransportUncertain,
    InvalidRemoteReceipt,
    ReceiptPersistenceFailed,
    RestartedAfterBeforeSend,
    ReconciliationUnavailable,
    RemoteRefDidNotMatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRemoteIntentStateV1 {
    BeforeSend,
    OutcomeUnknown { reason: SyncRemoteUnknownReasonV1 },
    Completed { receipt: Box<SyncRemoteReceiptV1> },
}

impl SyncRemoteIntentStateV1 {
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    pub fn blocks_retry(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoteIntentRecordV1 {
    pub version: u8,
    pub request: SyncRemoteIntentRequestV1,
    pub state: SyncRemoteIntentStateV1,
}

impl SyncRemoteIntentRecordV1 {
    pub const VERSION: u8 = 1;

    pub(crate) fn before_send(request: SyncRemoteIntentRequestV1) -> Self {
        Self {
            version: Self::VERSION,
            request,
            state: SyncRemoteIntentStateV1::BeforeSend,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncRemoteValueErrorV1 {
    InvalidOperationId,
    InvalidIdempotencyKey,
    InvalidShareId,
    InvalidAccountProvider,
    InvalidAccountRegistrationId,
    InvalidReceiptId,
    InvalidDigest,
    InvalidCanonicalRemote,
    InvalidRemoteRef,
    InvalidGitObjectId,
}

impl fmt::Display for SyncRemoteValueErrorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidOperationId => "invalid sync remote operation id",
            Self::InvalidIdempotencyKey => "invalid sync remote idempotency key",
            Self::InvalidShareId => "invalid sync share id",
            Self::InvalidAccountProvider => "invalid sync account provider",
            Self::InvalidAccountRegistrationId => "invalid sync account registration id",
            Self::InvalidReceiptId => "invalid sync remote receipt id",
            Self::InvalidDigest => "invalid lowercase SHA-256 digest",
            Self::InvalidCanonicalRemote => "invalid canonical sync remote",
            Self::InvalidRemoteRef => "invalid exact sync remote ref",
            Self::InvalidGitObjectId => "invalid lowercase Git object id",
        })
    }
}

impl std::error::Error for SyncRemoteValueErrorV1 {}
