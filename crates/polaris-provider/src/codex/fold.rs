//! Incrementally folds Codex Responses API SSE events into one completion.
//!
//! Transport, retry, pacing, and request accounting remain in the parent
//! module. This module owns only framing-aware response interpretation.

use serde_json::Value;

use super::metrics;
use crate::web_search::{parse_hosted_web_search_item, parse_url_citations};
use crate::{CompletionResponse, ProviderError, ReasoningItem, ToolCall, sse};

/// SSE semantics. Framing is left to `sse::SseDecoder`; this only holds
/// event interpretation. Keeping it separate from HTTP means it can be
/// tested without a network.
pub struct Folder {
    pub(super) decoder: sse::SseDecoder,
    pub(super) text: String,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) completed: bool,
    pub(super) usage: Option<crate::Usage>,
    pub(super) reasoning: Vec<ReasoningItem>,
    pub(super) hosted_web_search: Vec<crate::web_search::HostedWebSearchItem>,
    pub(super) url_citations: Vec<crate::web_search::UrlCitation>,
    pub(super) metrics_usage: metrics::TokenUsage,
    pub(super) cancelled: bool,
    pub(super) received_bytes: usize,
    pub(super) failed: bool,
    pub(super) preview_text: String,
    pub(super) saw_delta: bool,
}

impl Default for Folder {
    fn default() -> Self {
        Self::new()
    }
}

impl Folder {
    pub fn new() -> Self {
        Self {
            decoder: sse::SseDecoder::new(),
            text: String::new(),
            tool_calls: Vec::new(),
            completed: false,
            usage: None,
            reasoning: Vec::new(),
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
            metrics_usage: metrics::TokenUsage::default(),
            cancelled: false,
            received_bytes: 0,
            failed: false,
            preview_text: String::new(),
            saw_delta: false,
        }
    }

    /// Pushes in a received byte fragment. Only complete events get
    /// interpreted.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProviderError> {
        self.push_observed(bytes, None)
    }

    pub(super) fn push_observed(
        &mut self,
        bytes: &[u8],
        observer: Option<&dyn crate::ResponseObserver>,
    ) -> Result<(), ProviderError> {
        if self.failed {
            return Err(ProviderError::Decode("SSE folder already failed".into()));
        }
        let result = (|| {
            self.received_bytes = crate::checked_size(
                self.received_bytes,
                bytes.len(),
                crate::MAX_RESPONSE_BYTES,
                "SSE response",
            )?;
            let mut decoder = std::mem::take(&mut self.decoder);
            let result = decoder.push_each(bytes, |ev| self.take_event(ev, observer));
            self.decoder = decoder;
            result
        })();
        self.failed = result.is_err();
        result
    }

    pub(crate) fn take_event(
        &mut self,
        ev: sse::SseEvent,
        observer: Option<&dyn crate::ResponseObserver>,
    ) -> Result<(), ProviderError> {
        let data = ev.data.trim();
        // A sentinel. Not JSON, so it isn't interpreted.
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Decode(format!("SSE data is not JSON: {e}")))?;

        if let Some(usage) = v.pointer("/response/usage") {
            self.metrics_usage = metrics::TokenUsage::parse(usage);
        }
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "response.output_item.done" => self.take_item(&v)?,
            "response.completed" => {
                self.completed = true;
                if std::env::var_os("POLARIS_DUMP_USAGE").is_some() {
                    eprintln!(
                        "[usage] {}",
                        v.pointer("/response/usage")
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| "<absent>".into())
                    );
                }
                self.usage = v.pointer("/response/usage").and_then(|u| {
                    let input_tokens = u.get("input_tokens")?.as_u64()? as u32;
                    let output_tokens = u.get("output_tokens")?.as_u64()? as u32;
                    let total_tokens = u.get("total_tokens")?.as_u64()? as u32;
                    let cached_tokens = u
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    Some(crate::Usage {
                        input_tokens,
                        output_tokens,
                        total_tokens,
                        cached_tokens,
                    })
                });
            }
            "response.failed" => {
                let msg = v
                    .pointer("/response/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("no reason given");
                return Err(ProviderError::Http(format!("the response failed: {msg}")));
            }
            "response.cancelled" => {
                self.cancelled = true;
                return Err(ProviderError::Http("the response was cancelled".into()));
            }
            "response.output_text.delta" if observer.is_some() => {
                let text = v
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ProviderError::Decode("text delta is missing".into()))?;
                let output_index = v
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ProviderError::Decode("delta output_index is missing".into()))?;
                let content_index =
                    v.get("content_index")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            ProviderError::Decode("delta content_index is missing".into())
                        })?;
                crate::checked_size(
                    self.preview_text.len(),
                    text.len(),
                    crate::MAX_TEXT_BYTES,
                    "text deltas",
                )?;
                self.preview_text.push_str(text);
                self.saw_delta = true;
                observer
                    .unwrap()
                    .try_emit(crate::ResponseEvent::TextDelta {
                        output_index,
                        content_index,
                        text,
                    })
                    .map_err(|reason| ProviderError::Observation {
                        reason,
                        completed: None,
                    })?;
            }
            // Without an observer, deltas remain ignored. Looking only at
            // finalized items gives the same result, and doesn't drag
            // in reassembly failure as a new way to break.
            _ => {}
        }
        Ok(())
    }

    pub(super) fn preview_matches(&self) -> Option<bool> {
        self.saw_delta.then_some(self.preview_text == self.text)
    }

    fn take_item(&mut self, v: &Value) -> Result<(), ProviderError> {
        if let Some(item) = parse_hosted_web_search_item(v)
            .map_err(|error| ProviderError::Decode(error.to_string()))?
        {
            self.hosted_web_search.push(item);
            return Ok(());
        }
        let Some(item) = v.get("item") else {
            return Ok(());
        };
        match item
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
        {
            "message" => {
                if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                    for p in parts {
                        self.url_citations.extend(parse_url_citations(p));
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            crate::checked_size(
                                self.text.len(),
                                t.len(),
                                crate::MAX_TEXT_BYTES,
                                "response text",
                            )?;
                            self.text.push_str(t);
                        }
                    }
                }
            }
            "function_call" => {
                crate::check_limit(
                    self.tool_calls.len() + 1,
                    crate::MAX_TOOL_CALLS,
                    "tool calls",
                )?;
                let id = item
                    .get("call_id")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::Decode("function_call has no call_id".into()))?;
                let name = item
                    .get("name")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::Decode("function_call has no name".into()))?;
                let raw = item
                    .get("arguments")
                    .and_then(|s| s.as_str())
                    .unwrap_or("{}");
                crate::check_limit(raw.len(), crate::MAX_ARGUMENT_BYTES, "tool arguments")?;
                let arguments: Value = serde_json::from_str(raw).map_err(|e| {
                    ProviderError::Decode(format!("function_call's arguments is not JSON: {e}"))
                })?;
                self.tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
            "reasoning" => {
                if let (Some(id), Some(encrypted_content)) = (
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty()),
                    item.get("encrypted_content")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty()),
                ) {
                    self.reasoning.push(ReasoningItem {
                        id: id.to_string(),
                        encrypted_content: encrypted_content.to_string(),
                    });
                }
                // A missing (or empty-string) `id`/`encrypted_content` -
                // `include` wasn't honored, a non-reasoning model, etc. -
                // is silently skipped. Continuity here is a best-effort
                // optimization the turn doesn't depend on; it's fine to
                // move on to the next turn without it.
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether `response.completed` has arrived. The websocket transport
    /// reads frames one at a time and needs to know when to stop; the HTTP
    /// path learns the same thing from the stream ending.
    pub(crate) fn is_complete(&self) -> bool {
        self.completed
    }

    /// What the provider reported for this response, for the request record.
    pub(crate) fn usage(&self) -> metrics::TokenUsage {
        self.metrics_usage
    }

    /// Returns the folded result. If completion was never observed, this
    /// is a hard failure.
    pub fn finish(self) -> Result<CompletionResponse, ProviderError> {
        if self.failed || !self.decoder.is_empty() || !self.completed {
            return Err(ProviderError::Decode(
                "the stream ended without ever seeing response.completed".into(),
            ));
        }
        Ok(CompletionResponse {
            text: self.text,
            tool_calls: self.tool_calls,
            reasoning: self.reasoning,
            usage: self.usage,
            hosted_web_search: self.hosted_web_search,
            url_citations: self.url_citations,
        })
    }
}
