//! Hosted web-search request policy and Responses-style output parsing.
//!
//! This module deliberately has no transport dependency. The Codex transport
//! must opt into a verified endpoint before it adds these fields to a request;
//! an unverified ChatGPT backend is not treated as compatible with the public
//! Responses API.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Whether a request may use the hosted web-search tool.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchPolicy {
    /// Preserve the existing request exactly.
    #[default]
    Disabled,
    /// Let the provider search only cached material. This still sends a
    /// provider request; it does not mean local offline operation.
    Cached,
    /// Permit the provider to access the live web.
    Live,
}

/// Evidence that the selected endpoint accepts Polaris's bounded web-search
/// request contract. `Unverified` is the safe default.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchCapability {
    #[default]
    Unverified,
    VerifiedResponsesContract,
}

/// Limits that have to be accepted as one request contract. They are opt-in:
/// a caller cannot claim a bounded request by setting only one of the fields.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct WebSearchRequestCaps {
    pub max_tool_calls: u32,
    pub max_output_tokens: u32,
}

impl WebSearchRequestCaps {
    pub fn new(max_tool_calls: u32, max_output_tokens: u32) -> Result<Self, WebSearchError> {
        if max_tool_calls == 0 {
            return Err(WebSearchError::InvalidCaps(
                "max_tool_calls must be greater than zero".into(),
            ));
        }
        if max_output_tokens == 0 {
            return Err(WebSearchError::InvalidCaps(
                "max_output_tokens must be greater than zero".into(),
            ));
        }
        Ok(Self {
            max_tool_calls,
            max_output_tokens,
        })
    }
}

/// Explicit hosted-tool configuration, kept outside `CompletionRequest` so
/// existing callers retain their literal request shape.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct WebSearchConfig {
    pub policy: WebSearchPolicy,
    pub capability: WebSearchCapability,
    pub caps: WebSearchRequestCaps,
}

impl WebSearchConfig {
    pub fn disabled() -> Self {
        Self {
            policy: WebSearchPolicy::Disabled,
            capability: WebSearchCapability::Unverified,
            caps: WebSearchRequestCaps {
                max_tool_calls: 1,
                max_output_tokens: 1,
            },
        }
    }

    pub fn cached(caps: WebSearchRequestCaps) -> Self {
        Self {
            policy: WebSearchPolicy::Cached,
            capability: WebSearchCapability::Unverified,
            caps,
        }
    }

    pub fn live(caps: WebSearchRequestCaps) -> Self {
        Self {
            policy: WebSearchPolicy::Live,
            capability: WebSearchCapability::Unverified,
            caps,
        }
    }

    pub fn with_verified_responses_contract(mut self) -> Self {
        self.capability = WebSearchCapability::VerifiedResponsesContract;
        self
    }

    /// Adds the complete public Responses request contract. Disabled is a
    /// strict no-op, so callers can assert byte-for-byte legacy wire equality.
    ///
    /// No compatibility retry is available: an endpoint which does not accept
    /// these fields is unsupported until its contract is verified.
    pub fn apply_to_body(self, body: &mut Value) -> Result<(), WebSearchError> {
        if self.policy == WebSearchPolicy::Disabled {
            return Ok(());
        }
        if self.capability != WebSearchCapability::VerifiedResponsesContract {
            return Err(WebSearchError::UnsupportedEndpoint);
        }

        let object = body.as_object_mut().ok_or_else(|| {
            WebSearchError::Malformed("request body must be a JSON object".into())
        })?;
        let tools = object
            .entry("tools")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| WebSearchError::Malformed("tools must be an array".into()))?;
        tools.push(serde_json::json!({
            "type": "web_search",
            "external_web_access": self.policy == WebSearchPolicy::Live,
        }));

        let include = object
            .entry("include")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| WebSearchError::Malformed("include must be an array".into()))?;
        let sources = Value::String("web_search_call.action.sources".into());
        if !include.iter().any(|value| value == &sources) {
            include.push(sources);
        }
        object.insert("max_tool_calls".into(), self.caps.max_tool_calls.into());
        object.insert(
            "max_output_tokens".into(),
            self.caps.max_output_tokens.into(),
        );
        Ok(())
    }
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebSearchError {
    #[error(
        "web search is unsupported until this endpoint accepts the verified Responses contract"
    )]
    UnsupportedEndpoint,
    #[error("invalid web-search caps: {0}")]
    InvalidCaps(String),
    #[error("malformed web-search payload: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostedWebSearchItem {
    pub output_index: u64,
    pub id: String,
    pub status: HostedItemStatus,
    pub action: WebSearchAction,
    pub sources: Vec<WebSource>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum HostedItemStatus {
    InProgress,
    Completed,
    Incomplete,
    Unknown(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum WebSearchAction {
    Search { queries: Vec<String> },
    OpenPage { url: String },
    FindInPage { url: String, pattern: String },
    Unknown(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WebSource {
    pub url: String,
}

/// A URL citation attached to an output-text part. Offsets are retained only
/// when both values are present and ordered; their unit is not inferred.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UrlCitation {
    pub url: String,
    pub title: Option<String>,
    pub start_index: Option<u64>,
    pub end_index: Option<u64>,
}

impl UrlCitation {
    pub fn has_safe_offsets(&self) -> bool {
        match (self.start_index, self.end_index) {
            (Some(start), Some(end)) => start <= end,
            _ => false,
        }
    }
}

/// Parses a finalized hosted web-search item. Function calls and unrelated
/// output items intentionally return `Ok(None)` so transports never dispatch
/// a hosted item as a local tool call.
pub fn parse_hosted_web_search_item(
    event: &Value,
) -> Result<Option<HostedWebSearchItem>, WebSearchError> {
    let item = match event.get("item") {
        Some(item) => item,
        None => return Ok(None),
    };
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if kind != "web_search_call" {
        return Ok(None);
    }
    let id = required_string(item, "id", "web_search_call.id")?;
    let output_index = event
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| WebSearchError::Malformed("web_search_call has no output_index".into()))?;
    let status = match item.get("status").and_then(Value::as_str) {
        Some("in_progress") => HostedItemStatus::InProgress,
        Some("completed") => HostedItemStatus::Completed,
        Some("incomplete") => HostedItemStatus::Incomplete,
        Some(other) => HostedItemStatus::Unknown(other.to_string()),
        None => {
            return Err(WebSearchError::Malformed(
                "web_search_call has no status".into(),
            ));
        }
    };
    let action_value = item
        .get("action")
        .ok_or_else(|| WebSearchError::Malformed("web_search_call has no action".into()))?;
    let action_type = required_string(action_value, "type", "web_search_call.action.type")?;
    let action = match action_type.as_str() {
        "search" => WebSearchAction::Search {
            queries: action_value
                .get("queries")
                .and_then(Value::as_array)
                .map(|queries| {
                    queries
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        },
        "open_page" => WebSearchAction::OpenPage {
            url: required_string(action_value, "url", "web_search_call.action.url")?,
        },
        "find_in_page" => WebSearchAction::FindInPage {
            url: required_string(action_value, "url", "web_search_call.action.url")?,
            pattern: required_string(action_value, "pattern", "web_search_call.action.pattern")?,
        },
        other => WebSearchAction::Unknown(other.to_string()),
    };
    let sources = action_value
        .get("sources")
        .and_then(Value::as_array)
        .map(|sources| {
            sources
                .iter()
                .filter_map(|source| source.get("url").and_then(Value::as_str))
                .map(|url| WebSource {
                    url: url.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(HostedWebSearchItem {
        output_index,
        id,
        status,
        action,
        sources,
    }))
}

/// Parses URL citations from one output-text part without rewriting its text.
/// Consumers may fall back to an end-of-message source list when offsets are
/// absent or have an unknown unit.
pub fn parse_url_citations(part: &Value) -> Vec<UrlCitation> {
    part.get("annotations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|annotation| annotation.get("type").and_then(Value::as_str) == Some("url_citation"))
        .filter_map(|annotation| {
            annotation
                .get("url")
                .and_then(Value::as_str)
                .map(|url| UrlCitation {
                    url: url.to_string(),
                    title: annotation
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    start_index: annotation.get("start_index").and_then(Value::as_u64),
                    end_index: annotation.get("end_index").and_then(Value::as_u64),
                })
        })
        .collect()
}

fn required_string(value: &Value, key: &str, label: &str) -> Result<String, WebSearchError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| WebSearchError::Malformed(format!("{label} is missing or empty")))
}
