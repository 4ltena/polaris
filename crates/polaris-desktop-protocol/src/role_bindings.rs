//! Durable local role choices. Values are metadata, never endpoint authority.
use crate::ids::DecimalU64;
use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_ROLE_BINDINGS: usize = 32;
const MAX_ROLE_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 512;
const MAX_ENDPOINT_BYTES: usize = 256;

/// Where a bound model runs. Wider than `LocalProvider`, which describes a
/// discovered local server and has no way to say "the model the run's own
/// registration already reaches". Kept separate from it so that widening
/// what a binding may name does not widen what local discovery reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundRuntime {
    Ollama,
    LmStudio,
    /// `llama-server` started by polaris itself. Unlike the other two it
    /// has no separate installation for the person to point at, so the
    /// settings screen shows it without an endpoint field.
    LlamaCpp,
    Account,
}

/// Mirrors the weights a type may declare. One entry per weight at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundTier {
    /// The old three spellings are kept as aliases: a binding saved by an
    /// earlier build spells them that way, and a desktop that cannot read
    /// its own saved bindings loses the roles silently. They map by
    /// position — the old top is now `supergiant` — and `giant` is new
    /// ground between the middle and the top.
    #[serde(alias = "low")]
    Dwarf,
    #[serde(alias = "medium")]
    Subgiant,
    Giant,
    #[serde(alias = "ultra")]
    Supergiant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedToolSupport {
    Supported,
    Unsupported,
    Unknown,
}

const MAX_EFFORT_BYTES: usize = 16;

fn bounded<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if value.is_empty() || value.len() > N || value.chars().any(char::is_control) {
        return Err(de::Error::custom("role binding text bounds"));
    }
    Ok(value)
}

/// An absent value is allowed; a present one is held to the same bounds, so
/// `null` and `""` do not become two ways of saying the same thing.
fn optional_bounded<'de, D: Deserializer<'de>, const N: usize>(
    d: D,
) -> Result<Option<String>, D::Error> {
    let Some(value) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > N || value.chars().any(char::is_control) {
        return Err(de::Error::custom("role binding text bounds"));
    }
    Ok(Some(value))
}

fn tier_bindings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<TierBinding>, D::Error> {
    let values = Vec::<TierBinding>::deserialize(d)?;
    let mut seen = std::collections::HashSet::new();
    if values.iter().any(|v| !seen.insert(v.tier)) {
        return Err(de::Error::custom("tier binding identity"));
    }
    Ok(values)
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBinding {
    #[serde(deserialize_with="bounded::<_, MAX_ROLE_BYTES>")]
    pub role: String,
    pub runtime: BoundRuntime,
    /// Absent for `account`, which has no endpoint of its own.
    #[serde(default, skip_serializing_if="Option::is_none",
            deserialize_with="optional_bounded::<_, MAX_ENDPOINT_BYTES>")]
    pub endpoint: Option<String>,
    #[serde(deserialize_with="bounded::<_, MAX_MODEL_BYTES>")]
    pub model: String,
    /// Absent leaves the backend's own default in place.
    #[serde(default, skip_serializing_if="Option::is_none",
            deserialize_with="optional_bounded::<_, MAX_EFFORT_BYTES>")]
    pub effort: Option<String>,
    pub observed_tool_support: ObservedToolSupport,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierBinding {
    pub tier: BoundTier,
    pub runtime: BoundRuntime,
    #[serde(default, skip_serializing_if="Option::is_none",
            deserialize_with="optional_bounded::<_, MAX_ENDPOINT_BYTES>")]
    pub endpoint: Option<String>,
    #[serde(deserialize_with="bounded::<_, MAX_MODEL_BYTES>")]
    pub model: String,
    #[serde(default, skip_serializing_if="Option::is_none",
            deserialize_with="optional_bounded::<_, MAX_EFFORT_BYTES>")]
    pub effort: Option<String>,
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
    /// What each declared weight is worth. Absent means the run prices no
    /// weight and every delegation keeps the parent's model.
    #[serde(default, skip_serializing_if="Vec::is_empty",
            deserialize_with="tier_bindings")]
    pub tier_bindings: Vec<TierBinding>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBindingsConfigured {
    pub configuration_revision: DecimalU64,
    #[serde(deserialize_with="bindings")]
    pub bindings: Vec<RoleBinding>,
    /// What each declared weight is worth. Absent means the run prices no
    /// weight and every delegation keeps the parent's model.
    #[serde(default, skip_serializing_if="Vec::is_empty",
            deserialize_with="tier_bindings")]
    pub tier_bindings: Vec<TierBinding>,
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
                "role": "reviewer", "runtime": "ollama",
                "endpoint": "http://127.0.0.1:11434/", "model": "qwen:small",
                "observed_tool_support": "supported"
            }], "tier_bindings": [{
                "tier": "dwarf", "runtime": "account",
                "model": "gpt-5.4-mini", "effort": "low"
            }]}
        });
        let request: Request = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), value);
        // A binding saved by a build that predates the rename still reads.
        // Dropping the aliases would lose a desktop's roles in silence, and
        // the canonical spelling above cannot catch that.
        for (stored, tier) in [
            ("low", BoundTier::Dwarf),
            ("medium", BoundTier::Subgiant),
            ("ultra", BoundTier::Supergiant),
        ] {
            assert_eq!(
                serde_json::from_value::<BoundTier>(serde_json::json!(stored)).expect(stored),
                tier
            );
        }
        let binding = RoleBinding {
            role: "reviewer".into(),
            runtime: BoundRuntime::Ollama,
            endpoint: Some("http://127.0.0.1:11434/".into()),
            model: "qwen:small".into(),
            effort: None,
            observed_tool_support: ObservedToolSupport::Supported,
        };
        let response = Response::for_request(
            &request,
            Ok(SuccessResult::SessionRolesConfigure(
                RoleBindingsConfigured {
                    configuration_revision: DecimalU64::new(5),
                    bindings: vec![binding],
                    tier_bindings: vec![TierBinding {
                        tier: BoundTier::Dwarf,
                        runtime: BoundRuntime::Account,
                        endpoint: None,
                        model: "gpt-5.4-mini".into(),
                        effort: Some("low".into()),
                    }],
                },
            )),
        )
        .unwrap();
        response.validate_for(&request).unwrap();
        let mut bad = value.clone();
        bad["params"]["bindings"][0]["extra"] = json!(true);
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // One entry per weight: two would leave which model it buys up to
        // whichever the reader happened to keep.
        let mut twice = value.clone();
        let duplicate = twice["params"]["tier_bindings"][0].clone();
        twice["params"]["tier_bindings"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(serde_json::from_value::<Request>(twice).is_err());
        // An account model has no endpoint, and a blank one is not a way
        // of saying so.
        let mut blank = value;
        blank["params"]["tier_bindings"][0]["endpoint"] = json!("");
        assert!(serde_json::from_value::<Request>(blank).is_err());
    }
}
