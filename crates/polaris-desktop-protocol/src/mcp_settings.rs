//! Host MCP registry and write-only secrets. Registry objects are validated by
//! the service's registration types; this wire never returns secret values.
use crate::ids::DecimalU64;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Map, Value};

pub const MAX_SERVERS: usize = 32;
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

fn servers<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Map<String, Value>>, D::Error> {
    let values = Vec::deserialize(d)?;
    if values.len() > MAX_SERVERS {
        return Err(de::Error::custom("mcp server count"));
    }
    Ok(values)
}

pub(crate) fn reference<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if !(1..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(de::Error::custom("mcp secret reference"));
    }
    Ok(value)
}

fn bounded<'de, D: Deserializer<'de>>(d: D, max: usize) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(transparent)]
    struct Reference(#[serde(deserialize_with = "reference")] String);
    let values = Vec::<Reference>::deserialize(d)?;
    if values.len() > max {
        return Err(de::Error::custom("mcp identifier count"));
    }
    Ok(values.into_iter().map(|r| r.0).collect())
}

fn references<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    bounded(d, 4096)
}

fn server_ids<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    bounded(d, MAX_SERVERS)
}

/// Serialized only by a trusted client; Debug never shows the value.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        if value.is_empty()
            || value.len() > MAX_SECRET_BYTES
            || value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
        {
            return Err(de::Error::custom("mcp secret value"));
        }
        Ok(Self(value))
    }
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpRegistrySave {
    pub expected_revision: DecimalU64,
    #[serde(deserialize_with = "servers")]
    pub servers: Vec<Map<String, Value>>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSecretSet {
    pub expected_revision: DecimalU64,
    #[serde(deserialize_with = "reference")]
    pub reference: String,
    pub value: SecretValue,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSecretDelete {
    pub expected_revision: DecimalU64,
    #[serde(deserialize_with = "reference")]
    pub reference: String,
}
}

crate::object_wire! {
/// Saved documents and this owner's startup revisions, which stay in effect
/// until the next owner start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpRegistryState {
    pub registry_revision: DecimalU64,
    #[serde(deserialize_with = "servers")]
    pub servers: Vec<Map<String, Value>>,
    pub secrets_revision: DecimalU64,
    #[serde(deserialize_with = "references")]
    pub secret_references: Vec<String>,
    pub active_registry_revision: DecimalU64,
    pub active_secrets_revision: DecimalU64,
    /// This owner's project servers with no current catalog index entry.
    #[serde(deserialize_with = "server_ids")]
    pub unchecked_servers: Vec<String>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalogTarget {
    #[serde(deserialize_with = "reference")]
    pub server_id: String,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCatalogOutcome {
    Checked,
    Partial,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCatalogFailure {
    Unavailable,
    Auth,
    Transport,
    Protection,
    Limit,
    StopUnconfirmed,
    Storage,
}

crate::object_wire! {
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckedWire {
    #[serde(deserialize_with = "reference")]
    server_id: String,
    outcome: McpCatalogOutcome,
    tools: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    failure: Option<McpCatalogFailure>,
}
}

/// A failure reason is present exactly when the check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCatalogChecked {
    pub server_id: String,
    pub outcome: McpCatalogOutcome,
    pub tools: DecimalU64,
    pub failure: Option<McpCatalogFailure>,
}

fn checked_valid(
    outcome: McpCatalogOutcome,
    tools: DecimalU64,
    failure: Option<McpCatalogFailure>,
) -> bool {
    failure.is_some() == (outcome == McpCatalogOutcome::Failed)
        && tools.get() <= 512
        && (tools.get() == 0
            || matches!(
                outcome,
                McpCatalogOutcome::Checked | McpCatalogOutcome::Partial
            ))
}

impl Serialize for McpCatalogChecked {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if !checked_valid(self.outcome, self.tools, self.failure) {
            return Err(serde::ser::Error::custom("mcp catalog failure reason"));
        }
        CheckedWire {
            server_id: self.server_id.clone(),
            outcome: self.outcome,
            tools: self.tools,
            failure: self.failure,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for McpCatalogChecked {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = CheckedWire::deserialize(d)?;
        if !checked_valid(w.outcome, w.tools, w.failure) {
            return Err(de::Error::custom("mcp catalog failure reason"));
        }
        Ok(Self {
            server_id: w.server_id,
            outcome: w.outcome,
            tools: w.tools,
            failure: w.failure,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCatalogCancelState {
    Cancelling,
    NotRunning,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalogCancelled {
    #[serde(deserialize_with = "reference")]
    pub server_id: String,
    pub state: McpCatalogCancelState,
}
}

pub const MAX_SETUP_CANDIDATES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSetupTransport {
    Stdio,
    StreamableHttp,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSetupReason {
    LauncherNotSupported,
    EnvironmentReference,
    CredentialInArguments,
    UnsupportedTransport,
    UnsupportedAuth,
    CommandNotFound,
    UnsupportedInstallLocation,
    AlreadyRegistered,
    Invalid,
}

fn bounded_text<'de, D: Deserializer<'de>>(d: D, max: usize) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty() || value.len() > max {
        return Err(de::Error::custom("mcp setup text"));
    }
    Ok(value)
}

fn name<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_text(d, 256)
}

fn description<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    bounded_text(d, 16 * 1024)
}

/// An importable candidate carries the untruncated approval text; any other
/// candidate carries only its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpSetupStatus {
    Importable {
        #[serde(deserialize_with = "description")]
        description: String,
    },
    Unavailable {
        reason: McpSetupReason,
    },
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSetupCandidate {
    #[serde(deserialize_with = "reference")]
    pub candidate_id: String,
    #[serde(deserialize_with = "name")]
    pub name: String,
    pub transport: McpSetupTransport,
    pub status: McpSetupStatus,
}
}

fn candidates<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<McpSetupCandidate>, D::Error> {
    let values = Vec::deserialize(d)?;
    if values.len() > MAX_SETUP_CANDIDATES {
        return Err(de::Error::custom("mcp setup candidate count"));
    }
    Ok(values)
}

crate::object_wire! {
/// A new detection replaces every earlier candidate ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSetupCandidates {
    #[serde(deserialize_with = "candidates")]
    pub candidates: Vec<McpSetupCandidate>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSetupTarget {
    #[serde(deserialize_with = "reference")]
    pub candidate_id: String,
}
}
