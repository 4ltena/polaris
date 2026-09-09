//! 役割ごとに異なる不透明IDと、精度を失わない正規十進u64文字列を定義する。

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{error::Error, fmt};

pub const MAX_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueError {
    InvalidId,
    InvalidDecimal,
    Overflow,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidId => "ID must contain 1..=128 UTF-8 bytes",
            Self::InvalidDecimal => "expected canonical decimal u64 string",
            Self::Overflow => "u64 overflow",
        })
    }
}
impl Error for ValueError {}

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ValueError> {
                let value = value.into();
                if value.is_empty() || value.len() > MAX_ID_BYTES {
                    return Err(ValueError::InvalidId);
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(d)?).map_err(de::Error::custom)
            }
        }
    )+};
}

ids!(
    ProjectId,
    SessionId,
    ClientId,
    RequestId,
    EngineEpoch,
    RunId,
    AttemptId,
    TaskId,
    SubscriptionId,
    ApprovalId,
    MessageId,
    AttachmentId,
    SnapshotId,
    HistoryCursor,
    OperationId,
    ResultId
);

/// revision、連番、offset、期限、件数のwire表現。加算はoverflowを明示する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DecimalU64(u64);

impl DecimalU64 {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    pub fn checked_add(self, amount: u64) -> Result<Self, ValueError> {
        self.0
            .checked_add(amount)
            .map(Self)
            .ok_or(ValueError::Overflow)
    }
}

impl Serialize for DecimalU64 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(de::Error::custom(ValueError::InvalidDecimal));
        }
        value
            .parse::<u64>()
            .map(Self)
            .map_err(|_| de::Error::custom(ValueError::Overflow))
    }
}
