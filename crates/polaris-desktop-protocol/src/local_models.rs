//! Bounded inventory observations, never model availability or execution authority.
use crate::ids::DecimalU64;
use serde::{Deserialize, Deserializer, Serialize, de};
pub const MAX_MODELS: usize = 1024;
pub const MAX_TEXT_BYTES: usize = 512;
pub const MAX_ENDPOINT_BYTES: usize = 256;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalProvider {
    Ollama,
    Lmstudio,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalAvailability {
    Available,
    Unreachable,
    InvalidResponse,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedCapability {
    Supported,
    Unsupported,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedExecutionLocation {
    Remote,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObservedLoadState {
    Loaded,
    Unloaded,
    #[default]
    Unknown,
}
fn text(s: &str, limit: usize) -> bool {
    !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control)
}
crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModels {
    pub provider: LocalProvider,
    #[serde(deserialize_with="endpoint")]
    pub endpoint: String,
}
}
fn endpoint<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if !text(&value, MAX_ENDPOINT_BYTES) || value.chars().any(char::is_whitespace) {
        return Err(de::Error::custom("endpoint bound"));
    }
    Ok(value)
}
fn metadata<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = String::deserialize(d)?;
    if !text(&value, MAX_TEXT_BYTES) {
        return Err(de::Error::custom("metadata bound"));
    }
    Ok(value)
}
fn digest<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    metadata(d).map(Some)
}
fn optional_metadata<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    metadata(d).map(Some)
}
crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModel {
    #[serde(deserialize_with="metadata")]
    pub model_id: String,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="digest")]
    pub digest: Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="optional_metadata")]
    pub variant: Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="crate::present")]
    pub max_context_length: Option<DecimalU64>,
    pub completion: ObservedCapability,
    pub tools: ObservedCapability,
    pub vision: ObservedCapability,
    pub reasoning: ObservedCapability,
    pub execution_location: ObservedExecutionLocation,
    /// Older peers did not report runtime residency.  Its absence is unknown,
    /// never an inference that the model is unloaded.
    #[serde(default)]
    pub load_state: ObservedLoadState,
}
}
crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelsResult {
    pub provider: LocalProvider,
    #[serde(deserialize_with="endpoint")]
    pub endpoint: String,
    pub availability: LocalAvailability,
    #[serde(deserialize_with="models")]
    pub models: Vec<LocalModel>,
}
}
fn models<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<LocalModel>, D::Error> {
    let models = Vec::<LocalModel>::deserialize(d)?;
    let mut ids = std::collections::HashSet::new();
    if models.len() > MAX_MODELS || models.iter().any(|m| !ids.insert(&m.model_id)) {
        return Err(de::Error::custom("model count/identity"));
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec,
        request::Request,
        response::{Response, SuccessResult},
    };
    use serde_json::json;

    fn request() -> serde_json::Value {
        json!({"protocol_version":1,"kind":"request","client_id":"client","request_id":"request","session_id":"session","method":"local.models","params":{"provider":"ollama","endpoint":"http://127.0.0.1:11434/"}})
    }
    #[test]
    fn local_models_strict_request_and_correlation() {
        let value = request();
        let req: Request = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), value);
        for (key, replacement) in [
            ("session_id", json!(null)),
            ("method", json!("session.snapshot")),
        ] {
            let mut bad = value.clone();
            bad[key] = replacement;
            assert!(serde_json::from_value::<Request>(bad).is_err());
        }
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove("session_id");
        assert!(serde_json::from_value::<Request>(missing).is_err());
        for extra in [
            json!({"provider":"openai","endpoint":"http://127.0.0.1/"}),
            json!({"provider":"ollama","endpoint":"http://127.0.0.1/","token":"x"}),
        ] {
            let mut bad = value.clone();
            bad["params"] = extra;
            assert!(serde_json::from_value::<Request>(bad).is_err());
        }
        let result = LocalModelsResult {
            provider: LocalProvider::Ollama,
            endpoint: "http://127.0.0.1:11434/".into(),
            availability: LocalAvailability::Available,
            models: vec![],
        };
        let response =
            Response::for_request(&req, Ok(SuccessResult::LocalModels(result.clone()))).unwrap();
        response.validate_for(&req).unwrap();
        let mut wrong = result;
        wrong.provider = LocalProvider::Lmstudio;
        assert!(Response::for_request(&req, Ok(SuccessResult::LocalModels(wrong))).is_err());
    }
    #[test]
    fn local_models_metadata_bounds_and_optional_digest() {
        let model = LocalModel {
            model_id: "m".into(),
            digest: None,
            variant: None,
            max_context_length: None,
            completion: ObservedCapability::Unknown,
            tools: ObservedCapability::Unknown,
            vision: ObservedCapability::Unknown,
            reasoning: ObservedCapability::Unknown,
            execution_location: ObservedExecutionLocation::Unknown,
            load_state: ObservedLoadState::Unknown,
        };
        assert_eq!(
            serde_json::to_value(model).unwrap(),
            json!({"model_id":"m","completion":"unknown","tools":"unknown","vision":"unknown","reasoning":"unknown","execution_location":"unknown","load_state":"unknown"})
        );
        let legacy: LocalModel = serde_json::from_value(json!({
            "model_id":"legacy", "completion":"unknown", "tools":"unknown",
            "vision":"unknown", "reasoning":"unknown", "execution_location":"unknown"
        }))
        .unwrap();
        assert_eq!(legacy.load_state, ObservedLoadState::Unknown);
        for bad in [
            json!({"model_id":"m","digest":null}),
            json!({"model_id":"x".repeat(513)}),
            json!({"model_id":"m","digest":"x".repeat(513)}),
            json!({"model_id":"m","extra":0}),
            json!({"model_id":"m","load_state":"warming"}),
        ] {
            assert!(serde_json::from_value::<LocalModel>(bad).is_err());
        }
        for models in [
            vec![json!({"model_id":"m"}); 2],
            (0..1025)
                .map(|i| json!({"model_id":i.to_string()}))
                .collect(),
        ] {
            assert!(
                serde_json::from_value::<LocalModelsResult>(
                    json!({"provider":"ollama","endpoint":"http://127.0.0.1/","models":models})
                )
                .is_err()
            );
        }
        assert!(codec::from_json::<LocalModel>(br#"{"model_id":"a","model_id":"b"}"#).is_err());
    }
    #[test]
    fn local_models_full_response_escape_budget() {
        let req: Request = serde_json::from_value(request()).unwrap();
        let result = LocalModelsResult {
            provider: LocalProvider::Ollama,
            endpoint: "http://127.0.0.1:11434/".into(),
            availability: LocalAvailability::Available,
            models: (0..1024)
                .map(|i| LocalModel {
                    model_id: format!("{i:04}{}", "\"".repeat(508)),
                    digest: Some("\"".repeat(512)),
                    variant: None,
                    max_context_length: None,
                    completion: ObservedCapability::Unknown,
                    tools: ObservedCapability::Unknown,
                    vision: ObservedCapability::Unknown,
                    reasoning: ObservedCapability::Unknown,
                    execution_location: ObservedExecutionLocation::Unknown,
                    load_state: ObservedLoadState::Unknown,
                })
                .collect(),
        };
        let response = Response::for_request(&req, Ok(SuccessResult::LocalModels(result))).unwrap();
        assert_eq!(
            codec::encode(&response).unwrap_err(),
            codec::CodecError::FrameTooLarge
        );
    }
}
