//! Durable role choices and past metadata observations, never runtime authorization.
//! Loading or validating these values performs no I/O and creates no provider.
use super::{StoreError, StoreResult};
use polaris_provider::local::{Capability, Endpoint, ModelSelection, Runtime};
use polaris_skills::AgentType;
use serde::{Deserialize, Serialize};

pub const MAX_ROLE_BINDINGS: usize = 32;
pub const MAX_ROLE_BYTES: usize = 128;
pub const MAX_MODEL_BYTES: usize = 512;

/// Where a bound model runs. The two local runtimes each answer at an
/// endpoint the user gave; `Account` is a model reached through the
/// registration the run already holds, which has no endpoint to store and
/// no credential of its own here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedRuntime {
    Ollama,
    LmStudio,
    Account,
}

impl SavedRuntime {
    fn wants_endpoint(self) -> bool {
        !matches!(self, Self::Account)
    }
}

/// The reasoning efforts the wire accepts. Stored as a closed set rather
/// than free text: an effort the backend does not know is refused at the
/// first request, and the settings screen is where that should be caught.
pub const EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// An observation only: Supported is neither a current capability nor a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSupport {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRoleBinding {
    pub role: String,
    pub runtime: SavedRuntime,
    /// Canonical, explicit IP-literal loopback origin; no embedded
    /// credentials. Absent for `Account`, which has no endpoint of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub model: String,
    /// `None` leaves the backend's own default in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub observed_tool_support: ToolSupport,
}

impl SavedRoleBinding {
    /// Copies metadata, not content identity, execution location or permission.
    pub fn capture(role: impl Into<String>, selection: &ModelSelection) -> StoreResult<Self> {
        let saved = Self {
            role: role.into(),
            runtime: match selection.runtime() {
                Runtime::Ollama => SavedRuntime::Ollama,
                Runtime::LmStudio => SavedRuntime::LmStudio,
            },
            endpoint: Some(selection.endpoint().as_str().to_owned()),
            model: selection.model().id().to_owned(),
            effort: None,
            observed_tool_support: match selection.model().capabilities().tools {
                Capability::Supported => ToolSupport::Supported,
                Capability::Unsupported => ToolSupport::Unsupported,
                Capability::Unknown => ToolSupport::Unknown,
            },
        };
        saved.validate()?;
        Ok(saved)
    }

    fn validate(&self) -> StoreResult<()> {
        bounded_text(&self.role, MAX_ROLE_BYTES)?;
        bounded_text(&self.model, MAX_MODEL_BYTES)?;
        validate_endpoint(self.runtime, self.endpoint.as_deref())?;
        validate_effort(self.effort.as_deref())
    }
}

/// A local runtime without an endpoint has nowhere to send, and an account
/// model with one would claim an origin nobody authorized. Neither is a
/// value the settings screen should be able to save.
fn validate_endpoint(runtime: SavedRuntime, endpoint: Option<&str>) -> StoreResult<()> {
    match (runtime.wants_endpoint(), endpoint) {
        (true, Some(value)) => {
            let parsed =
                Endpoint::parse(value).map_err(|_| StoreError::Corrupt("role endpoint"))?;
            if parsed.as_str() != value {
                return Err(StoreError::Corrupt("role endpoint normalization"));
            }
            Ok(())
        }
        (false, None) => Ok(()),
        _ => Err(StoreError::Corrupt("role endpoint presence")),
    }
}

fn validate_effort(effort: Option<&str>) -> StoreResult<()> {
    match effort {
        None => Ok(()),
        Some(value) if EFFORTS.contains(&value) => Ok(()),
        Some(_) => Err(StoreError::Corrupt("effort")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRoleBindings {
    pub schema_version: u32,
    pub bindings: Vec<SavedRoleBinding>,
}

impl SavedRoleBindings {
    /// An empty explicit set remains empty; the owner must not infer fallback.
    pub fn new(mut bindings: Vec<SavedRoleBinding>) -> StoreResult<Self> {
        if bindings.len() > MAX_ROLE_BINDINGS {
            return Err(StoreError::Overflow);
        }
        for binding in &mut bindings {
            if let Some(endpoint) = &binding.endpoint {
                binding.endpoint = Some(
                    Endpoint::parse(endpoint)
                        .map_err(|_| StoreError::Corrupt("role endpoint"))?
                        .as_str()
                        .to_owned(),
                );
            }
        }
        bindings.sort_by(|a, b| a.role.cmp(&b.role));
        let saved = Self {
            schema_version: 1,
            bindings,
        };
        saved.validate()?;
        Ok(saved)
    }

    /// Call after deserialization and before publication. Does not refresh any
    /// observation or resolve models, credentials, tools or execution grants.
    pub fn validate(&self) -> StoreResult<()> {
        if self.schema_version != 1 {
            return Err(StoreError::UnsupportedVersion(self.schema_version));
        }
        if self.bindings.len() > MAX_ROLE_BINDINGS {
            return Err(StoreError::Overflow);
        }
        for binding in &self.bindings {
            binding.validate()?;
        }
        if self
            .bindings
            .windows(2)
            .any(|pair| pair[0].role >= pair[1].role)
        {
            return Err(StoreError::Corrupt("role order or duplicate"));
        }
        Ok(())
    }

    /// Checks each configured role against a trusted current catalog, with no
    /// discovery or file reads. Unconfigured catalog roles remain unconfigured.
    /// Catalog membership alone does not authorize sending data to an endpoint.
    pub fn validate_catalog(&self, catalog: &[AgentType]) -> StoreResult<()> {
        self.validate()?;
        for binding in &self.bindings {
            if catalog
                .iter()
                .filter(|agent| agent.name == binding.role)
                .count()
                != 1
            {
                return Err(StoreError::Corrupt("role catalog mismatch"));
            }
        }
        Ok(())
    }
}

/// What one declared weight is worth. Separate from a role binding because
/// the two answer different questions: a role names the child doing the
/// work, a weight is how much any child's work is expected to need. A type
/// declares its own weight in `SKILL.md`, so nothing here overrides that —
/// this only says what the weight buys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedTierBinding {
    pub tier: SavedTier,
    pub runtime: SavedRuntime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Mirrors `polaris_skills::Tier` on the wire. Kept as its own type so the
/// stored spelling cannot drift if the in-memory enum is ever reordered or
/// given a different serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedTier {
    /// `low` is kept as an alias because bindings saved before the rungs
    /// were renamed spell it that way, and a saved binding that stops
    /// deserializing is a role the desktop silently loses. The old three map
    /// onto the new four by position, not by name: what used to be the top
    /// is now `supergiant`, and `giant` is new ground between it and the
    /// middle.
    #[serde(alias = "low")]
    Dwarf,
    #[serde(alias = "medium")]
    Subgiant,
    Giant,
    #[serde(alias = "ultra")]
    Supergiant,
}

impl From<polaris_skills::Tier> for SavedTier {
    fn from(tier: polaris_skills::Tier) -> Self {
        match tier {
            polaris_skills::Tier::Dwarf => Self::Dwarf,
            polaris_skills::Tier::Subgiant => Self::Subgiant,
            polaris_skills::Tier::Giant => Self::Giant,
            polaris_skills::Tier::Supergiant => Self::Supergiant,
        }
    }
}

impl From<SavedTier> for polaris_skills::Tier {
    fn from(tier: SavedTier) -> Self {
        match tier {
            SavedTier::Dwarf => Self::Dwarf,
            SavedTier::Subgiant => Self::Subgiant,
            SavedTier::Giant => Self::Giant,
            SavedTier::Supergiant => Self::Supergiant,
        }
    }
}

impl SavedTierBinding {
    fn validate(&self) -> StoreResult<()> {
        bounded_text(&self.model, MAX_MODEL_BYTES)?;
        validate_endpoint(self.runtime, self.endpoint.as_deref())?;
        validate_effort(self.effort.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedTierBindings {
    #[serde(default)]
    pub bindings: Vec<SavedTierBinding>,
}

impl SavedTierBindings {
    /// At most one entry per weight. Two entries for the same weight would
    /// leave which model it buys up to iteration order.
    pub fn new(mut bindings: Vec<SavedTierBinding>) -> StoreResult<Self> {
        for binding in &mut bindings {
            if let Some(endpoint) = &binding.endpoint {
                binding.endpoint = Some(
                    Endpoint::parse(endpoint)
                        .map_err(|_| StoreError::Corrupt("tier endpoint"))?
                        .as_str()
                        .to_owned(),
                );
            }
        }
        bindings.sort_by_key(|binding| binding.tier);
        let saved = Self { bindings };
        saved.validate()?;
        Ok(saved)
    }

    pub fn validate(&self) -> StoreResult<()> {
        for binding in &self.bindings {
            binding.validate()?;
        }
        if self
            .bindings
            .windows(2)
            .any(|pair| pair[0].tier >= pair[1].tier)
        {
            return Err(StoreError::Corrupt("tier order or duplicate"));
        }
        Ok(())
    }
}

fn bounded_text(value: &str, max: usize) -> StoreResult<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(StoreError::Corrupt("role text bounds"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "roles/tests.rs"]
mod tests;
