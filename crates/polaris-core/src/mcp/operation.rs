//! Host-bound MCP operation evidence.
//!
//! This module deliberately has no deserialization boundary and no grant
//! store.  It turns an already prepared `BoundOperation` into evidence for the
//! existing Gate and desktop operation owner; it never authorizes a call.

use polaris_mcp::{BoundOperation, OperationBinding, catalog::Kind, registration::canonical_hash};
use serde_json::json;

use crate::desktop_store::BootstrapTier;

const MAX_DISPLAY_BYTES: usize = 256;
/// Arguments are shown whole on the approval card; larger calls are refused.
const MAX_ARGUMENT_DISPLAY_BYTES: usize = 16 * 1024;

pub(crate) mod read;
mod saved;
pub use saved::SavedMcpIntent;

/// Explicit host ceiling for an operation that can have an external effect.
///
/// This is intentionally separate from `SandboxPolicy`: confined code can be
/// configured for either a non-external or external desktop tier.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExternalCeiling {
    Denied,
    Allowed,
}

impl ExternalCeiling {
    pub const fn host_external(allowed: bool) -> Self {
        if allowed { Self::Allowed } else { Self::Denied }
    }

    pub const fn desktop_tier(tier: BootstrapTier) -> Self {
        match tier {
            BootstrapTier::ReadCreateBuildExternal => Self::Allowed,
            BootstrapTier::ReadOnly
            | BootstrapTier::ReadCreate
            | BootstrapTier::ReadCreateBuild => Self::Denied,
        }
    }

    pub const fn permits_external(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Generic MCP tools are always unknown-impact.  Server annotations remain
/// untrusted display material and cannot construct this value differently.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum McpImpact {
    Unknown,
}

impl McpImpact {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
        }
    }
}

/// Identifiers selected by the host at a run boundary, not by model JSON.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HostOperationIdentity {
    pub run_id: String,
    pub attempt_id: String,
    pub model_call_id: String,
    pub operation_id: String,
    pub policy_revision: u64,
    pub expires_at_ms: u64,
}

/// Readable, secret-safe details for an approval UI or owner audit entry.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct McpApprovalMetadata {
    pub server_id: String,
    pub kind: &'static str,
    pub item: String,
    pub target: String,
    pub impact: McpImpact,
    pub payload_hash: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct McpApprovalDisplay {
    pub title: &'static str,
    pub description: String,
    pub metadata: McpApprovalMetadata,
}

/// Evidence for exactly one host-owned MCP operation.
///
/// It is intentionally neither `Clone` nor `Deserialize`.  A matching hash is
/// not a grant, and callers still must pass the same object through `Gate` and
/// the existing desktop owner before `perform_once` can send bytes.
#[derive(Debug)]
pub struct McpOperation {
    binding: OperationBinding,
    identity: HostOperationIdentity,
    target: String,
    payload_hash: String,
    meta_arguments_hash: Option<String>,
    call: Option<(String, Option<&'static super::apps::App>)>,
}

impl McpOperation {
    /// Trusted host construction from a prepared transport operation.
    /// Model input cannot reach this constructor through serialization.
    pub fn from_bound(
        identity: HostOperationIdentity,
        target: impl Into<String>,
        operation: &BoundOperation,
    ) -> Result<Self, String> {
        Self::from_binding(identity, target, operation.binding().clone())
    }

    /// This is crate-private so only the host integration can adapt an
    /// already-validated transport binding.  It is not an authorization API.
    pub(crate) fn from_binding(
        identity: HostOperationIdentity,
        target: impl Into<String>,
        binding: OperationBinding,
    ) -> Result<Self, String> {
        if binding.item.kind != Kind::Tool {
            return Err("mcp_operation_not_tool".into());
        }
        let target = checked_host_text(target.into(), "target")?;
        for (name, value) in [
            ("run ID", &identity.run_id),
            ("attempt ID", &identity.attempt_id),
            ("model call ID", &identity.model_call_id),
            ("operation ID", &identity.operation_id),
        ] {
            checked_host_text(value.clone(), name)?;
        }
        let payload_hash = canonical_hash(&payload_value(&identity, &binding, &target))
            .map_err(|_| "mcp_operation_payload_limit")?;
        Ok(Self {
            binding,
            identity,
            target,
            payload_hash,
            meta_arguments_hash: None,
            call: None,
        })
    }

    pub(crate) fn from_resource_binding(
        identity: HostOperationIdentity,
        target: impl Into<String>,
        binding: OperationBinding,
        meta_arguments_hash: String,
    ) -> Result<Self, String> {
        read::check_resource_binding(&binding)?;
        if !saved::valid_hash(&meta_arguments_hash) {
            return Err("mcp_invalid_resource_call".into());
        }
        let target = checked_host_text(target.into(), "target")?;
        for value in [
            &identity.run_id,
            &identity.attempt_id,
            &identity.model_call_id,
            &identity.operation_id,
        ] {
            checked_host_text(value.clone(), "identity")?;
        }
        let mut payload = payload_value(&identity, &binding, &target);
        payload["impact"] = json!("resource_read");
        payload["meta_arguments_hash"] = json!(meta_arguments_hash);
        let payload_hash = canonical_hash(&payload).map_err(|_| "mcp_operation_payload_limit")?;
        Ok(Self {
            binding,
            identity,
            target,
            payload_hash,
            meta_arguments_hash: Some(meta_arguments_hash),
            call: None,
        })
    }

    /// Attach the exact arguments the binding hashed, for display only. The
    /// payload hash already binds them; a mismatch or oversize is refused.
    pub(crate) fn with_arguments(
        mut self,
        arguments: &serde_json::Value,
        app: Option<&'static super::apps::App>,
    ) -> Result<Self, String> {
        if canonical_hash(arguments).ok().as_ref() != Some(&self.binding.arguments_hash) {
            return Err("mcp_arguments_mismatch".into());
        }
        let text = serde_json::to_string(arguments).map_err(|_| "mcp_arguments_mismatch")?;
        if text.len() > MAX_ARGUMENT_DISPLAY_BYTES {
            return Err("mcp_arguments_too_large".into());
        }
        self.call = Some((text, app));
        Ok(self)
    }

    pub(crate) fn app(&self) -> Option<&'static super::apps::App> {
        self.call.as_ref()?.1
    }

    pub(crate) fn is_resource_read(&self) -> bool {
        self.meta_arguments_hash.is_some()
    }

    pub fn binding(&self) -> &OperationBinding {
        &self.binding
    }

    pub fn identity(&self) -> &HostOperationIdentity {
        &self.identity
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub const fn impact(&self) -> McpImpact {
        McpImpact::Unknown
    }

    pub fn payload_hash(&self) -> &str {
        &self.payload_hash
    }

    pub fn approval_display(&self) -> McpApprovalDisplay {
        let metadata = McpApprovalMetadata {
            server_id: readable(&self.binding.item.server_id),
            kind: "tool",
            item: readable(&self.binding.item.key),
            target: readable(&self.target),
            impact: self.impact(),
            payload_hash: self.payload_hash.clone(),
        };
        let view = self.call.as_ref().and_then(|(arguments, app)| {
            let arguments = serde_json::from_str(arguments).ok()?;
            Some(app.as_ref()?.view(&self.binding.item.key, &arguments))
        });
        let code = view
            .as_ref()
            .is_some_and(|v| v.class == super::apps::ToolClass::CodeExecution);
        let mut description = format!(
            "サーバー {} のツール {} を対象 {} へ実行します。",
            metadata.server_id, metadata.item, metadata.target,
        );
        if let Some(view) = &view {
            description.push_str(&format!(
                "\nアプリ: {}（アプリで現在開いている対象に作用します）",
                view.app
            ));
            if let Some(inner) = &view.inner {
                description.push_str(&format!("\n内側のツール: {inner}"));
            }
        }
        description.push_str(if code {
            "\n影響: 任意のコードを実行します。呼び出すたびに確認します。"
        } else {
            "\n影響: 未判定"
        });
        if let Some((arguments, _)) = &self.call {
            description.push_str(&format!("\n引数: {arguments}"));
        }
        McpApprovalDisplay {
            title: if code {
                "MCP 任意コード実行"
            } else {
                "MCP ツール実行"
            },
            description,
            metadata,
        }
    }
}

fn payload_value(
    identity: &HostOperationIdentity,
    binding: &OperationBinding,
    target: &str,
) -> serde_json::Value {
    // Keep every transport binding field explicit, so adding one requires a
    // conscious hash-contract update and a test change.
    json!({
        "attempt_id": identity.attempt_id,
        "binding": {
            "arguments_hash": binding.arguments_hash,
            "catalog_generation": binding.catalog_generation,
            "connection_generation": binding.connection_generation,
            "destination_hash": binding.destination_hash,
            "item": {
                "key": binding.item.key,
                "kind": binding.item.kind,
                "server_id": binding.item.server_id,
            },
            "registry_revision": binding.registry_revision,
            "resource_uri": binding.resource_uri,
            "schema_hash": binding.schema_hash,
            "selection_id": binding.selection_id,
        },
        "expires_at_ms": identity.expires_at_ms,
        "impact": McpImpact::Unknown.as_str(),
        "model_call_id": identity.model_call_id,
        "operation_id": identity.operation_id,
        "policy_revision": identity.policy_revision,
        "run_id": identity.run_id,
        "target": target,
    })
}

fn checked_host_text(value: String, _name: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > MAX_DISPLAY_BYTES || value.chars().any(char::is_control) {
        return Err("mcp_invalid_host_identity".into());
    }
    Ok(value)
}

fn readable(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            '\u{fffd}'
        } else {
            character
        };
        if result.len() + character.len_utf8() > MAX_DISPLAY_BYTES {
            while result.len() > MAX_DISPLAY_BYTES - '…'.len_utf8() {
                result.pop();
            }
            result.push('…');
            break;
        }
        result.push(character);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_mcp::catalog::ItemId;

    fn identity() -> HostOperationIdentity {
        HostOperationIdentity {
            run_id: "run-1".into(),
            attempt_id: "attempt-1".into(),
            model_call_id: "call-1".into(),
            operation_id: "operation-1".into(),
            policy_revision: 7,
            expires_at_ms: 1234,
        }
    }

    fn binding() -> OperationBinding {
        OperationBinding {
            item: ItemId {
                server_id: "server-1".into(),
                kind: Kind::Tool,
                key: "apply".into(),
            },
            registry_revision: 3,
            destination_hash: "destination".into(),
            connection_generation: "connection".into(),
            catalog_generation: "catalog".into(),
            schema_hash: Some("schema".into()),
            arguments_hash: "arguments".into(),
            selection_id: Some("selection".into()),
            resource_uri: Some("resource".into()),
        }
    }

    fn hash(identity: HostOperationIdentity, binding: OperationBinding, target: &str) -> String {
        McpOperation::from_binding(identity, target, binding)
            .unwrap()
            .payload_hash
    }

    #[test]
    fn payload_hash_binds_every_host_and_transport_field() {
        let baseline = hash(identity(), binding(), "project/file");
        let mut cases = Vec::new();
        let mut changed = identity();
        changed.run_id.push('x');
        cases.push(hash(changed, binding(), "project/file"));
        let mut changed = identity();
        changed.attempt_id.push('x');
        cases.push(hash(changed, binding(), "project/file"));
        let mut changed = identity();
        changed.model_call_id.push('x');
        cases.push(hash(changed, binding(), "project/file"));
        let mut changed = identity();
        changed.operation_id.push('x');
        cases.push(hash(changed, binding(), "project/file"));
        let mut changed = identity();
        changed.policy_revision += 1;
        cases.push(hash(changed, binding(), "project/file"));
        let mut changed = identity();
        changed.expires_at_ms += 1;
        cases.push(hash(changed, binding(), "project/file"));
        let mut changed = binding();
        changed.item.server_id.push('x');
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.item.key.push('x');
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.registry_revision += 1;
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.destination_hash.push('x');
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.connection_generation.push('x');
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.catalog_generation.push('x');
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.schema_hash = None;
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.arguments_hash.push('x');
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.selection_id = None;
        cases.push(hash(identity(), changed, "project/file"));
        let mut changed = binding();
        changed.resource_uri = None;
        cases.push(hash(identity(), changed, "project/file"));
        cases.push(hash(identity(), binding(), "project/other"));
        assert!(cases.into_iter().all(|changed| changed != baseline));

        let mut non_tool = binding();
        non_tool.item.kind = Kind::Resource;
        assert!(McpOperation::from_binding(identity(), "project/file", non_tool).is_err());
    }

    #[test]
    fn canonical_payload_has_stable_object_key_order() {
        let operation = McpOperation::from_binding(identity(), "project/file", binding()).unwrap();
        let mut reordered = serde_json::Map::new();
        reordered.insert("target".into(), json!(operation.target()));
        reordered.insert("run_id".into(), json!(operation.identity().run_id));
        reordered.insert(
            "policy_revision".into(),
            json!(operation.identity().policy_revision),
        );
        reordered.insert(
            "operation_id".into(),
            json!(operation.identity().operation_id),
        );
        reordered.insert(
            "model_call_id".into(),
            json!(operation.identity().model_call_id),
        );
        reordered.insert("impact".into(), json!("unknown"));
        reordered.insert(
            "expires_at_ms".into(),
            json!(operation.identity().expires_at_ms),
        );
        reordered.insert("attempt_id".into(), json!(operation.identity().attempt_id));
        reordered.insert(
            "binding".into(),
            payload_value(
                operation.identity(),
                operation.binding(),
                operation.target(),
            )["binding"]
                .clone(),
        );
        assert_eq!(
            operation.payload_hash(),
            canonical_hash(&serde_json::Value::Object(reordered)).unwrap()
        );
    }

    #[test]
    fn generic_tools_are_unknown_and_display_never_contains_arguments_or_destination() {
        let operation = McpOperation::from_binding(identity(), "project/file", binding()).unwrap();
        let display = operation.approval_display();
        assert_eq!(operation.impact(), McpImpact::Unknown);
        assert_eq!(display.metadata.impact, McpImpact::Unknown);
        assert!(!display.description.contains("arguments"));
        assert!(!display.description.contains("destination"));
    }

    #[test]
    fn attached_arguments_are_shown_whole_only_when_they_are_the_hashed_ones() {
        let arguments = json!({"code": "import bpy\nbpy.ops.mesh.primitive_cube_add()"});
        let mut bound = binding();
        bound.item.key = "execute_blender_code".into();
        bound.arguments_hash = canonical_hash(&arguments).unwrap();
        let plain = McpOperation::from_binding(identity(), "project", bound.clone()).unwrap();
        let hash = plain.payload_hash().to_owned();
        let blender = &crate::mcp::apps::APPS[0];
        let operation = plain.with_arguments(&arguments, Some(blender)).unwrap();
        assert_eq!(operation.payload_hash(), hash);
        let display = operation.approval_display();
        assert_eq!(display.title, "MCP 任意コード実行");
        assert!(display.description.contains("アプリ: Blender"));
        assert!(
            display
                .description
                .contains(&serde_json::to_string(&arguments).unwrap())
        );
        let other = McpOperation::from_binding(identity(), "project", bound.clone()).unwrap();
        assert!(
            other
                .with_arguments(&json!({"code": "changed"}), None)
                .is_err()
        );
        let large = json!({"code": "x".repeat(MAX_ARGUMENT_DISPLAY_BYTES)});
        bound.arguments_hash = canonical_hash(&large).unwrap();
        let other = McpOperation::from_binding(identity(), "project", bound).unwrap();
        assert_eq!(
            other.with_arguments(&large, None).unwrap_err(),
            "mcp_arguments_too_large"
        );
    }

    #[test]
    fn display_bounds_multibyte_and_control_characters() {
        for input in ["x".repeat(1000), "あ".repeat(1000), "\n".repeat(1000)] {
            let label = readable(&input);
            assert!(label.len() <= MAX_DISPLAY_BYTES);
            assert!(!label.chars().any(char::is_control));
            assert!(label.ends_with('…'));
        }
        assert_eq!(readable("合成サーバー"), "合成サーバー");
        for input in [
            "x".repeat(253),
            "x".repeat(254),
            "x".repeat(256),
            format!("{}あ", "x".repeat(253)),
        ] {
            assert_eq!(readable(&input), input);
        }
    }

    #[test]
    fn external_ceiling_is_explicit_and_only_external_desktop_tier_permits_it() {
        assert_eq!(
            ExternalCeiling::host_external(false),
            ExternalCeiling::Denied
        );
        assert_eq!(
            ExternalCeiling::host_external(true),
            ExternalCeiling::Allowed
        );
        for tier in [
            BootstrapTier::ReadOnly,
            BootstrapTier::ReadCreate,
            BootstrapTier::ReadCreateBuild,
        ] {
            assert_eq!(ExternalCeiling::desktop_tier(tier), ExternalCeiling::Denied);
        }
        assert_eq!(
            ExternalCeiling::desktop_tier(BootstrapTier::ReadCreateBuildExternal),
            ExternalCeiling::Allowed
        );
    }
}
