//! Parser boundary for the root-only verification tool.

use crate::verification::{VerificationCheckpointActionV1, parse_tool_action};

pub(crate) fn parse(
    call: &polaris_provider::ToolCall,
) -> Result<VerificationCheckpointActionV1, String> {
    if call.name != "verification" {
        return Err("verification tool required".into());
    }
    parse_tool_action(&call.arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_rejects_model_controlled_execution_fields() {
        let call = polaris_provider::ToolCall {
            id: "id".into(),
            name: "verification".into(),
            arguments: serde_json::json!({"op":"run","preparation_id":"prepared-1","path":"/tmp/x"}),
        };
        assert!(parse(&call).is_err());
    }
}
