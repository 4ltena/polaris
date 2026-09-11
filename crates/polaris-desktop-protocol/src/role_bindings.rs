//! Durable local role choices. Values are metadata, never endpoint authority.
use crate::{ids::DecimalU64, local_models::LocalProvider};
use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_ROLE_BINDINGS: usize = 32;
const MAX_ROLE_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 512;
const MAX_ENDPOINT_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedToolSupport {
    Supported,
    Unsupported,
    Unknown,
}

fn bounded<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty() || value.len() > N || value.chars().any(char::is_control) {
        return Err(de::Error::custom("role binding text bounds"));
    }
    Ok(value)
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBinding {
    #[serde(deserialize_with="bounded::<_, MAX_ROLE_BYTES>")]
    pub role: String,
    pub provider: LocalProvider,
    #[serde(deserialize_with="bounded::<_, MAX_ENDPOINT_BYTES>")]
    pub endpoint: String,
    #[serde(deserialize_with="bounded::<_, MAX_MODEL_BYTES>")]
    pub model: String,
    pub observed_tool_support: ObservedToolSupport,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDescriptor {
    #[serde(deserialize_with="bounded::<_, MAX_ROLE_BYTES>")]
    pub role: String,
    pub requires_tools: bool,
}
}

fn bindings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<RoleBinding>, D::Error> {
    let values = Vec::<RoleBinding>::deserialize(d)?;
    let mut roles = std::collections::HashSet::new();
    if values.len() > MAX_ROLE_BINDINGS || values.iter().any(|v| !roles.insert(&v.role)) {
        return Err(de::Error::custom("role binding count/identity"));
    }
    Ok(values)
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureRoleBindings {
    pub expected_configuration_revision: DecimalU64,
    #[serde(deserialize_with="bindings")]
    pub bindings: Vec<RoleBinding>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBindingsConfigured {
    pub configuration_revision: DecimalU64,
    #[serde(deserialize_with="bindings")]
    pub bindings: Vec<RoleBinding>,
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        request::Request,
        response::{Response, SuccessResult},
    };
    use serde_json::json;

    #[test]
    fn role_configuration_is_strict_and_correlated() {
        let value = json!({
            "protocol_version": 1, "kind": "request", "client_id": "client",
            "request_id": "request", "session_id": "session",
            "method": "session.roles.configure",
            "params": {"expected_configuration_revision": "4", "bindings": [{
                "role": "reviewer", "provider": "ollama",
                "endpoint": "http://127.0.0.1:11434/", "model": "qwen:small",
                "observed_tool_support": "supported"
            }]}
        });
        let request: Request = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), value);
        let binding = RoleBinding {
            role: "reviewer".into(),
            provider: LocalProvider::Ollama,
            endpoint: "http://127.0.0.1:11434/".into(),
            model: "qwen:small".into(),
            observed_tool_support: ObservedToolSupport::Supported,
        };
        let response = Response::for_request(
            &request,
            Ok(SuccessResult::SessionRolesConfigure(
                RoleBindingsConfigured {
                    configuration_revision: DecimalU64::new(5),
                    bindings: vec![binding],
                },
            )),
        )
        .unwrap();
        response.validate_for(&request).unwrap();
        let mut bad = value;
        bad["params"]["bindings"][0]["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }
}
