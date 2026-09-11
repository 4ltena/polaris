//! Durable role choices and past metadata observations, never runtime authorization.
//! Loading or validating these values performs no I/O and creates no provider.
use super::{StoreError, StoreResult};
use polaris_provider::local::{Capability, Endpoint, ModelSelection, Runtime};
use polaris_skills::AgentType;
use serde::{Deserialize, Serialize};

pub const MAX_ROLE_BINDINGS: usize = 32;
pub const MAX_ROLE_BYTES: usize = 128;
pub const MAX_MODEL_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedRuntime {
    Ollama,
    LmStudio,
}

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
    /// Canonical, explicit IP-literal loopback origin; no embedded credentials.
    pub endpoint: String,
    pub model: String,
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
            endpoint: selection.endpoint().as_str().to_owned(),
            model: selection.model().id().to_owned(),
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
        let endpoint =
            Endpoint::parse(&self.endpoint).map_err(|_| StoreError::Corrupt("role endpoint"))?;
        if endpoint.as_str() != self.endpoint {
            return Err(StoreError::Corrupt("role endpoint normalization"));
        }
        Ok(())
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
            binding.endpoint = Endpoint::parse(&binding.endpoint)
                .map_err(|_| StoreError::Corrupt("role endpoint"))?
                .as_str()
                .to_owned();
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

fn bounded_text(value: &str, max: usize) -> StoreResult<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(StoreError::Corrupt("role text bounds"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "roles/tests.rs"]
mod tests;
