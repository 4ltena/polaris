//! Immutable evidence for a saved MCP operation, never a reconstructed grant.
use super::*;
use polaris_mcp::catalog::{ItemId, tool_alias};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedMcpIntent {
    pub(crate) run_id: String,
    pub(crate) attempt_id: String,
    pub(crate) model_call_id: String,
    pub(crate) operation_id: String,
    pub(crate) policy_revision: u64,
    pub(crate) expires_at_ms: u64,
    pub(crate) target: String,
    pub(crate) server_id: String,
    pub(crate) tool_name: String,
    pub(crate) registry_revision: u64,
    pub(crate) destination_hash: String,
    pub(crate) connection_generation: String,
    pub(crate) catalog_generation: String,
    pub(crate) schema_hash: Option<String>,
    pub(crate) arguments_hash: String,
    pub(crate) payload_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resource: Option<SavedResourceRead>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedResourceRead {
    pub(crate) kind: Kind,
    pub(crate) uri: String,
    pub(crate) meta_arguments_hash: String,
}

impl SavedMcpIntent {
    pub(crate) fn from_operation(operation: &McpOperation) -> Result<Self, String> {
        let identity = operation.identity();
        let binding = operation.binding();
        if !operation.is_resource_read()
            && (binding.item.kind != Kind::Tool
                || binding.selection_id.is_some()
                || binding.resource_uri.is_some())
        {
            return Err("mcp_invalid_saved_intent".into());
        }
        let saved = Self {
            run_id: identity.run_id.clone(),
            attempt_id: identity.attempt_id.clone(),
            model_call_id: identity.model_call_id.clone(),
            operation_id: identity.operation_id.clone(),
            policy_revision: identity.policy_revision,
            expires_at_ms: identity.expires_at_ms,
            target: operation.target().into(),
            server_id: binding.item.server_id.clone(),
            tool_name: binding.item.key.clone(),
            registry_revision: binding.registry_revision,
            destination_hash: binding.destination_hash.clone(),
            connection_generation: binding.connection_generation.clone(),
            catalog_generation: binding.catalog_generation.clone(),
            schema_hash: binding.schema_hash.clone(),
            arguments_hash: binding.arguments_hash.clone(),
            payload_hash: operation.payload_hash().into(),
            resource: operation
                .meta_arguments_hash
                .as_ref()
                .map(|hash| SavedResourceRead {
                    kind: binding.item.kind,
                    uri: binding.resource_uri.clone().expect("validated resource"),
                    meta_arguments_hash: hash.clone(),
                }),
        };
        saved.validate()?;
        Ok(saved)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !ascii_name(&self.server_id, 64, b"-_")
            || (self.resource.is_none()
                && (!ascii_name(&self.tool_name, 128, b"_.-")
                    || self
                        .schema_hash
                        .as_ref()
                        .is_none_or(|hash| !valid_hash(hash))))
            || self.connection_generation.len() != 32
            || !ascii_name(&self.connection_generation, 32, b"-_")
            || ![
                &self.destination_hash,
                &self.catalog_generation,
                &self.arguments_hash,
                &self.payload_hash,
            ]
            .into_iter()
            .all(|hash| valid_hash(hash))
        {
            return Err("mcp_invalid_saved_intent".into());
        }
        let identity = HostOperationIdentity {
            run_id: self.run_id.clone(),
            attempt_id: self.attempt_id.clone(),
            model_call_id: self.model_call_id.clone(),
            operation_id: self.operation_id.clone(),
            policy_revision: self.policy_revision,
            expires_at_ms: self.expires_at_ms,
        };
        // Recompute the original hash contract. This value is used only for
        // validation, and cannot reconstruct the consumed BoundOperation.
        let expected = match &self.resource {
            Some(resource) => McpOperation::from_resource_binding(
                identity,
                self.target.clone(),
                self.binding(),
                resource.meta_arguments_hash.clone(),
            )?,
            None => McpOperation::from_binding(identity, self.target.clone(), self.binding())?,
        };
        if expected.payload_hash() != self.payload_hash {
            return Err("mcp_saved_intent_hash_mismatch".into());
        }
        Ok(())
    }

    pub(crate) fn alias(&self) -> Result<String, String> {
        if self.resource.is_some() {
            return Ok("mcp_catalog".into());
        }
        tool_alias(&self.binding().item).map_err(|_| "mcp_invalid_tool_alias".into())
    }

    pub(crate) fn binding(&self) -> OperationBinding {
        OperationBinding {
            item: ItemId {
                server_id: self.server_id.clone(),
                kind: self.resource.as_ref().map_or(Kind::Tool, |read| read.kind),
                key: self.tool_name.clone(),
            },
            registry_revision: self.registry_revision,
            destination_hash: self.destination_hash.clone(),
            connection_generation: self.connection_generation.clone(),
            catalog_generation: self.catalog_generation.clone(),
            schema_hash: self.schema_hash.clone(),
            arguments_hash: self.arguments_hash.clone(),
            selection_id: None,
            resource_uri: self.resource.as_ref().map(|read| read.uri.clone()),
        }
    }
}

pub(super) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ascii_name(value: &str, limit: usize, punctuation: &[u8]) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || punctuation.contains(&byte))
}
