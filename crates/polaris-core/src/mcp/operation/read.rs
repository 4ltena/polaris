//! Exact resource meta-call binding; it cannot select prompts or grant tools.
use super::*;
use polaris_mcp::catalog::ItemId;
use polaris_provider::ToolCall;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceRead {
    op: String,
    server: String,
    kind: Kind,
    key: String,
    generation: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

fn empty_arguments() -> Value {
    json!({})
}

impl ResourceRead {
    pub(crate) fn parse(call: &ToolCall) -> Result<Self, String> {
        if call.name != "mcp_catalog" {
            return Err("mcp_invalid_resource_call".into());
        }
        canonical_hash(&call.arguments).map_err(|_| "mcp_invalid_resource_call")?;
        let read: Self = serde_json::from_value(call.arguments.clone())
            .map_err(|_| "mcp_invalid_resource_call")?;
        if read.op != "read"
            || !matches!(read.kind, Kind::Resource | Kind::ResourceTemplate)
            || !read.arguments.is_object()
        {
            return Err("mcp_invalid_resource_call".into());
        }
        Ok(read)
    }
    pub(crate) fn id(&self) -> ItemId {
        ItemId {
            server_id: self.server.clone(),
            kind: self.kind,
            key: self.key.clone(),
        }
    }
    pub(crate) fn generation(&self) -> &str {
        &self.generation
    }
    pub(crate) fn arguments(&self) -> &Value {
        &self.arguments
    }
    pub(crate) fn check(&self, binding: &OperationBinding) -> Result<(), String> {
        check_resource_binding(binding)?;
        if self.id() != binding.item
            || self.generation != binding.catalog_generation
            || canonical_hash(&self.arguments).map_err(|_| "mcp_invalid_resource_call")?
                != binding.arguments_hash
        {
            return Err("mcp_invalid_resource_call".into());
        }
        Ok(())
    }
}

pub(super) fn check_resource_binding(binding: &OperationBinding) -> Result<(), String> {
    let key = &binding.item.key;
    let uri = binding
        .resource_uri
        .as_deref()
        .ok_or("mcp_missing_resource_uri")?;
    if !matches!(binding.item.kind, Kind::Resource | Kind::ResourceTemplate)
        || binding.selection_id.is_some()
        || binding.schema_hash.is_some()
        || [key.as_str(), uri]
            .into_iter()
            .any(|v| v.is_empty() || v.len() > 16 * 1024 || v.chars().any(char::is_control))
        || (binding.item.kind == Kind::Resource
            && (uri != key
                || binding.arguments_hash
                    != canonical_hash(&json!({})).map_err(|_| "mcp_invalid_resource_call")?))
    {
        return Err("mcp_invalid_resource_binding".into());
    }
    Ok(())
}
