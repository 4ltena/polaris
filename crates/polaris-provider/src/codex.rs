//! A provider that speaks the Responses API using ChatGPT subscription
//! auth.
//!
//! The shape differs from `/chat/completions`. Tool definitions are flat
//! rather than nested, history is a sequence of `input` elements rather
//! than `messages`, and `arguments` is a string holding JSON rather than
//! JSON itself. This doesn't share functions with `openai.rs` so that
//! fixing one side can never silently break the other.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    CompletionRequest, CompletionResponse, Message, Provider, ProviderError, Role, ToolCall, sse,
};

/// The endpoint to send requests to. `store` is never used, so this is the
/// only one ever hit.
pub const ENDPOINT_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// The default used when `POLARIS_MODEL` is omitted.
///
/// `gpt-5.1-codex-max` / `gpt-5.2-codex` / `gpt-5.3-codex`, picked up from
/// strings in the binary at design time, existed in none of the real
/// catalog (`codex debug models`). The real backend clearly returned a 400
/// saying "Codex usage on a ChatGPT account is not supported" for these,
/// with auth itself going through fine (not a 401). Swapped in for the
/// real catalog's top-priority model instead. As the spec states plainly,
/// there's no guarantee this name will keep working going forward either.
pub const DEFAULT_MODEL: &str = "gpt-5.6-sol";

/// Converts history into a sequence of Responses `input` elements.
pub fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::User => out.push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": m.content }],
            })),
            Role::Assistant => {
                // A turn with empty body text and only tool calls isn't
                // unusual. Adding an empty message would just pile up
                // content-free utterances in the history.
                if !m.content.is_empty() {
                    out.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.content }],
                    }));
                }
                for c in &m.tool_calls {
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.name,
                        // A string holding JSON, not JSON itself.
                        "arguments": c.arguments.to_string(),
                    }));
                }
            }
            Role::Tool => out.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.content,
            })),
        }
    }
    out
}

/// Converts tool definitions into the Responses API's flat shape.
pub fn tool_wire_shape(tools: &[polaris_tools::ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect()
}

/// Assembles the request body.
///
/// `effort` is sent as `reasoning.effort`. When it's `None`, the
/// `reasoning` key itself is omitted rather than sent, leaving it to the
/// server's default — the same shape as the existing decision to omit the
/// `tools` key entirely rather than send an empty array.
pub fn build_body(model: &str, req: &CompletionRequest, effort: Option<&str>) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "instructions": req.system,
        "input": input_items(&req.messages),
        // Never let the server hold conversation state. Send the full
        // text every turn. What's sent and what's measured then match,
        // and the prefix never shifts under us.
        "store": false,
        "stream": true,
        // Without this the backend writes nothing to its prompt cache for
        // a reasoning model: measured on 2026-08-25, the same two-turn
        // run reported `cache_write_tokens: 0` and `cached_tokens: 0` on
        // every request without it, and 7,680 of 8,014 input tokens
        // served from cache on the second request with it. It is not
        // merely a request to be shown the reasoning — it is what makes
        // the turn cacheable at all, which is why it isn't something to
        // opt into. Behind a flag that defaults to off, the default run
        // is the one that pays full price for a prefix it already sent.
        "include": ["reasoning.encrypted_content"],
        // Points the backend at the shard already holding this prefix.
        // `store: false` keeps the prefix stable, but stability alone
        // doesn't help if each turn is routed somewhere else — see
        // `crate::cache_key`.
        "prompt_cache_key": crate::cache_key(model, effort, req),
    });
    let tools = tool_wire_shape(&req.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(e) = effort {
        body["reasoning"] = serde_json::json!({ "effort": e });
    }
    body
}

/// SSE semantics. Framing is left to `sse::SseDecoder`; this only holds
/// event interpretation. Keeping it separate from HTTP means it can be
/// tested without a network.
pub struct Folder {
    decoder: sse::SseDecoder,
    text: String,
    tool_calls: Vec<ToolCall>,
    completed: bool,
    usage: Option<crate::Usage>,
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
        }
    }

    /// Pushes in a received byte fragment. Only complete events get
    /// interpreted.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProviderError> {
        for ev in self.decoder.push(bytes) {
            let data = ev.data.trim();
            // A sentinel. Not JSON, so it isn't interpreted.
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let v: Value = serde_json::from_str(data)
                .map_err(|e| ProviderError::Decode(format!("SSE data is not JSON: {e}")))?;

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
                    return Err(ProviderError::Http("the response was cancelled".into()));
                }
                // Deltas and everything else are skipped. Looking only at
                // finalized items gives the same result, and doesn't drag
                // in reassembly failure as a new way to break.
                _ => {}
            }
        }
        Ok(())
    }

    fn take_item(&mut self, v: &Value) -> Result<(), ProviderError> {
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
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            self.text.push_str(t);
                        }
                    }
                }
            }
            "function_call" => {
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
                let arguments: Value = serde_json::from_str(raw).map_err(|e| {
                    ProviderError::Decode(format!("function_call's arguments is not JSON: {e}"))
                })?;
                self.tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Returns the folded result. If completion was never observed, this
    /// is a hard failure.
    pub fn finish(self) -> Result<CompletionResponse, ProviderError> {
        if !self.completed {
            return Err(ProviderError::Decode(
                "the stream ended without ever seeing response.completed".into(),
            ));
        }
        Ok(CompletionResponse {
            text: self.text,
            tool_calls: self.tool_calls,
            usage: self.usage,
        })
    }
}

/// Cut the connection once silence lasts this long. Measuring against the
/// whole response would cut off a normal long think.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub struct CodexProvider {
    base: String,
    // `RwLock`, not a plain `String`/`Option<String>` — `set_model`/
    // `set_effort` take `&self` so they can be called through the same
    // shared `&dyn Provider` the rest of the harness already holds (see
    // the trait's docs).
    model: std::sync::RwLock<String>,
    // An explicit `/model` effort choice, taking precedence over the
    // account-plan-derived `Token::effort` when set — see
    // `attempt`'s docs.
    effort_override: std::sync::RwLock<Option<String>>,
    tokens: Arc<dyn crate::TokenSource>,
    client: reqwest::Client,
    idle: Duration,
}

impl CodexProvider {
    pub fn new(base: String, model: String, tokens: Arc<dyn crate::TokenSource>) -> Self {
        Self::with_idle_timeout(base, model, tokens, DEFAULT_IDLE_TIMEOUT)
    }

    pub fn with_idle_timeout(
        base: String,
        model: String,
        tokens: Arc<dyn crate::TokenSource>,
        idle: Duration,
    ) -> Self {
        Self {
            base,
            model: std::sync::RwLock::new(model),
            effort_override: std::sync::RwLock::new(None),
            tokens,
            client: reqwest::Client::new(),
            idle,
        }
    }

    /// Sends a single request and folds the SSE. A 401 is not folded
    /// here; it's returned to the caller as-is so the caller can decide
    /// whether to retry.
    ///
    /// The body is assembled here. `effort` can vary per `token` (in
    /// practice a refreshed token never actually carries a different plan
    /// determination, but keeping the correspondence "this attempt's body
    /// uses the effort of the token this attempt used" is less error-prone
    /// than having the caller reuse a body across attempts) — but an
    /// explicit `set_effort` call (via `/model`) always wins over the
    /// token's plan-derived value when one has been made.
    async fn attempt(
        &self,
        token: &crate::Token,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let model = self.model.read().expect("model lock poisoned").clone();
        let override_effort = self
            .effort_override
            .read()
            .expect("effort lock poisoned")
            .clone();
        let effort = override_effort.as_deref().or(token.effort.as_deref());
        let body = build_body(&model, req, effort);
        let resp = self
            .client
            .post(format!("{}/responses", self.base))
            .bearer_auth(&token.access_token)
            .header("chatgpt-account-id", &token.account_id)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // Let the caller decide whether to refresh and retry.
            return Err(ProviderError::Auth(format!("status {status}")));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let hint = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http(format!(
                "rate limited. retry-after: {hint} seconds. {body}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http(format!("status {status}: {body}")));
        }

        let mut folder = Folder::new();
        let mut stream = resp.bytes_stream();
        loop {
            // Measured against idle time. The response's overall length
            // grows normally.
            let next = tokio::time::timeout(self.idle, stream.next()).await;
            match next {
                Err(_) => {
                    return Err(ProviderError::Http(format!(
                        "no response arrived for {} seconds",
                        self.idle.as_secs()
                    )));
                }
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    let bytes = chunk.map_err(|e| ProviderError::Http(e.to_string()))?;
                    folder.push(&bytes)?;
                }
            }
        }
        folder.finish()
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let token = self.tokens.token().await?;
        match self.attempt(&token, &req).await {
            Err(ProviderError::Auth(_)) => {
                // Exactly once. Never retry indefinitely.
                let token = self.tokens.refreshed().await?;
                self.attempt(&token, &req).await.map_err(|e| match e {
                    ProviderError::Auth(_) => ProviderError::Auth(
                        "auth was still refused after refreshing. redo `polaris login`".into(),
                    ),
                    other => other,
                })
            }
            other => other,
        }
    }

    fn set_model(&self, model: &str) {
        *self.model.write().expect("model lock poisoned") = model.to_string();
    }

    fn set_effort(&self, effort: Option<&str>) {
        *self.effort_override.write().expect("effort lock poisoned") = effort.map(str::to_string);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;

    fn frame(kind: &str, extra: Value) -> Vec<u8> {
        let mut v = serde_json::json!({ "type": kind });
        if let Some(o) = extra.as_object() {
            for (k, val) in o {
                v[k] = val.clone();
            }
        }
        format!("data: {v}\n\n").into_bytes()
    }

    fn message_item(text: &str) -> Value {
        serde_json::json!({
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }]
            }
        })
    }

    fn function_call_item(id: &str, name: &str, args: &str) -> Value {
        serde_json::json!({
            "item": { "type": "function_call", "call_id": id, "name": name, "arguments": args }
        })
    }

    #[test]
    fn a_text_only_stream_folds_into_text() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            message_item("42 lines"),
        ))
        .expect("push should succeed");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should be complete");
        assert_eq!(r.text, "42 lines");
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn a_function_call_item_becomes_a_tool_call() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            function_call_item("call_9", "read", r#"{"path":"a.txt"}"#),
        ))
        .expect("push should succeed");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should be complete");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "call_9");
        assert_eq!(r.tool_calls[0].name, "read");
        assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    }

    /// The result doesn't change no matter where the frame gets split.
    /// Split tolerance is `sse.rs`'s responsibility, but whether this
    /// path actually goes through it is confirmed separately here. If it
    /// didn't, this would be redoing the reassembly on its own.
    #[test]
    fn a_stream_split_mid_frame_folds_the_same_way() {
        let whole: Vec<u8> = frame("response.output_item.done", message_item("split tolerance"))
            .into_iter()
            .chain(frame("response.completed", serde_json::json!({})))
            .collect();

        for cut in 1..whole.len() {
            let mut f = Folder::new();
            f.push(&whole[..cut]).expect("push should succeed");
            f.push(&whole[cut..]).expect("push should succeed");
            let r = f.finish().expect("should be complete");
            assert_eq!(r.text, "split tolerance", "broke when split at byte {cut}");
        }
    }

    /// A response that ended without ever seeing `response.completed`
    /// must never be returned as a normal success. The agent loop reads
    /// the absence of a tool call as "done," so it would silently return
    /// an empty final answer.
    #[test]
    fn a_stream_that_never_completes_is_an_error() {
        let mut f = Folder::new();
        f.push(&frame("response.output_item.done", message_item("midway")))
            .expect("push should succeed");
        let err = f
            .finish()
            .expect_err("should fail since it never completed");
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "not Decode: {err:?}"
        );
    }

    /// Even with an empty body, if it completed, it's a success. Don't
    /// conflate an empty string with absence. This is the counterpart to
    /// the test above — without it, an implementation that "always fails"
    /// would pass.
    #[test]
    fn an_empty_but_completed_stream_is_a_success() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should succeed since it completed");
        assert!(r.text.is_empty());
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn usage_is_captured_from_the_completed_event() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.completed",
            serde_json::json!({
                "response": {
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 8,
                        "total_tokens": 20
                    }
                }
            }),
        ))
        .expect("push should succeed");

        let res = f.finish().expect("finish should succeed");
        let usage = res.usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.total_tokens, 20);
        assert_eq!(
            usage.cached_tokens, 0,
            "a completed event with no input_tokens_details must not fail to parse, just report 0"
        );
    }

    #[test]
    fn cached_tokens_is_parsed_from_input_tokens_details() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.completed",
            serde_json::json!({
                "response": {
                    "usage": {
                        "input_tokens": 9708,
                        "output_tokens": 167,
                        "total_tokens": 9875,
                        "input_tokens_details": {"cached_tokens": 5578}
                    }
                }
            }),
        ))
        .expect("push should succeed");

        let res = f.finish().expect("finish should succeed");
        let usage = res.usage.expect("usage should be present");
        assert_eq!(usage.cached_tokens, 5578);
    }

    #[test]
    fn a_completed_event_without_usage_yields_none_not_an_error() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");

        let res = f.finish().expect("finish should succeed");
        assert!(res.usage.is_none());
    }

    #[test]
    fn a_failed_response_carries_its_message() {
        let mut f = Folder::new();
        let err = f
            .push(&frame(
                "response.failed",
                serde_json::json!({ "response": { "error": { "message": "model overloaded" } } }),
            ))
            .expect_err("should fail");
        let ProviderError::Http(msg) = err else {
            panic!("not Http: {err:?}");
        };
        assert!(
            msg.contains("model overloaded"),
            "reason missing from message: {msg}"
        );
    }

    #[test]
    fn a_cancelled_response_is_an_error() {
        let mut f = Folder::new();
        let err = f
            .push(&frame("response.cancelled", serde_json::json!({})))
            .expect_err("should fail");
        assert!(matches!(err, ProviderError::Http(_)), "not Http: {err:?}");
    }

    /// Delta events are skipped. Picking them up and piling them on
    /// doubles the body text.
    #[test]
    fn delta_events_are_ignored() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_text.delta",
            serde_json::json!({ "delta": "duplicate" }),
        ))
        .expect("push should succeed");
        f.push(&frame(
            "response.output_item.done",
            message_item("duplicate"),
        ))
        .expect("push should succeed");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should complete");
        assert_eq!(
            r.text, "duplicate",
            "picked up the delta and double-counted it"
        );
    }

    /// A call whose `arguments` is broken as JSON is stopped here, before
    /// it reaches the dispatcher. Passing it through would look like "an
    /// unknown argument," and the cause couldn't be traced back.
    #[test]
    fn a_function_call_with_broken_arguments_is_a_decode_error() {
        let mut f = Folder::new();
        let err = f
            .push(&frame(
                "response.output_item.done",
                function_call_item("c", "read", "{ not json"),
            ))
            .expect_err("should fail");
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "not Decode: {err:?}"
        );
    }

    /// The `[DONE]` sentinel is not JSON. Don't crash trying to interpret
    /// it.
    #[test]
    fn the_done_sentinel_is_not_parsed_as_json() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        f.push(b"data: [DONE]\n\n")
            .expect("must not fail on the sentinel");
        let r = f.finish().expect("should complete");
        assert!(r.text.is_empty());
    }

    #[test]
    fn a_user_message_becomes_an_input_text_item() {
        let items = input_items(&[Message::user("hello")]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn an_assistant_message_becomes_an_output_text_item() {
        let items = input_items(&[Message::assistant("yes")]);
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
        assert_eq!(items[0]["content"][0]["text"], "yes");
    }

    /// A tool call becomes a `function_call`, and `arguments` is a string
    /// holding JSON, not JSON itself. Sending it as a raw Value here
    /// makes the server say the type is wrong and return a 400.
    #[test]
    fn a_tool_call_becomes_a_function_call_with_stringified_arguments() {
        let items = input_items(&[Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "Cargo.toml" }),
            }],
        )]);
        assert_eq!(
            items.len(),
            1,
            "an empty message got added even though the body was empty"
        );
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["name"], "read");
        let raw = items[0]["arguments"]
            .as_str()
            .expect("arguments is not a string");
        let parsed: Value = serde_json::from_str(raw).expect("arguments is not JSON");
        assert_eq!(parsed["path"], "Cargo.toml");
    }

    /// A turn carrying both body text and tool calls emits the message
    /// first and the function_call after. If the order were reversed, the
    /// model would see its own call before its own utterance.
    #[test]
    fn a_turn_with_both_text_and_calls_emits_the_message_first() {
        let items = input_items(&[Message::assistant_with_tool_calls(
            "I'll read it",
            vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
        )]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn a_tool_result_becomes_a_function_call_output() {
        let items = input_items(&[Message::tool_result("call_1", "42 lines")]);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["output"], "42 lines");
    }

    /// The Responses API's tool definitions are flat. Sending
    /// `/chat/completions`'s `{"type":"function","function":{…}}` gets
    /// rejected.
    #[test]
    fn tool_definitions_are_flat_not_nested() {
        let specs = polaris_tools::all_specs();
        let wire = tool_wire_shape(&specs);
        assert_eq!(wire.len(), specs.len());
        for (w, s) in wire.iter().zip(specs.iter()) {
            assert_eq!(w["type"], "function");
            assert_eq!(w["name"], s.name, "name is not placed flat");
            assert!(
                w.get("function").is_none(),
                "a nested function is still present"
            );
            assert!(w["description"].is_string());
            assert_eq!(w["parameters"], s.parameters);
        }
    }

    #[test]
    fn the_body_carries_instructions_and_never_stores_state() {
        let req = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("go")],
            tools: polaris_tools::all_specs(),
        };
        let body = build_body("gpt-5.3-codex", &req, None);

        assert_eq!(body["model"], "gpt-5.3-codex");
        assert_eq!(body["instructions"], "system");
        assert_eq!(
            body["store"], false,
            "the server is being made to hold conversation state"
        );
        assert_eq!(body["stream"], true);
        assert!(
            body.get("previous_response_id").is_none(),
            "conversation reuse is being used"
        );
        assert_eq!(
            body["input"]
                .as_array()
                .expect("input is not an array")
                .len(),
            1
        );
        assert_eq!(
            body["tools"]
                .as_array()
                .expect("tools is not an array")
                .len(),
            req.tools.len()
        );
        assert!(
            body.get("reasoning").is_none(),
            "reasoning is being sent even though effort wasn't passed"
        );
    }

    /// The point of the key: it must not move as the conversation grows.
    /// A key derived from the messages would change every turn and route
    /// each request to a fresh shard, which is exactly the 0% hit rate
    /// this was added to fix. Pinning turn 1 against turn 5 is what
    /// catches that regression; asserting the key merely exists would
    /// not.
    #[test]
    fn the_cache_key_holds_still_while_the_conversation_grows() {
        let turn = |n: usize| CompletionRequest {
            system: "system".into(),
            messages: (0..n).map(|i| Message::user(format!("turn {i}"))).collect(),
            tools: polaris_tools::all_specs(),
        };
        let first = build_body("m", &turn(1), Some("high"));
        let fifth = build_body("m", &turn(5), Some("high"));

        assert!(
            first["prompt_cache_key"].is_string(),
            "no prompt_cache_key is being sent"
        );
        assert_eq!(
            first["prompt_cache_key"], fifth["prompt_cache_key"],
            "the key moved as the conversation grew, so every turn routes elsewhere"
        );
    }

    /// The other half of the pair. Everything the key covers is something
    /// that breaks prefix reuse, so a request that changes one of them
    /// must not be pointed at the shard holding the old prefix — it would
    /// only evict it. Without this, a constant key would pass the test
    /// above.
    #[test]
    fn anything_that_breaks_the_prefix_changes_the_cache_key() {
        let req = |system: &str, tools: Vec<polaris_tools::ToolSpec>| CompletionRequest {
            system: system.into(),
            messages: vec![Message::user("go")],
            tools,
        };
        let all = polaris_tools::all_specs();
        let base =
            build_body("m", &req("system", all.clone()), Some("high"))["prompt_cache_key"].clone();

        for (what, other) in [
            (
                "the model",
                build_body("other", &req("system", all.clone()), Some("high")),
            ),
            (
                "the effort",
                build_body("m", &req("system", all.clone()), Some("low")),
            ),
            (
                "the system prompt",
                build_body("m", &req("other", all.clone()), Some("high")),
            ),
            (
                "the tool list",
                build_body("m", &req("system", all[1..].to_vec()), Some("high")),
            ),
        ] {
            assert_ne!(
                base, other["prompt_cache_key"],
                "changing {what} left the cache key alone"
            );
        }
    }

    /// Not a cosmetic request to be shown the model's reasoning. Without
    /// it the backend writes nothing to its prompt cache for a reasoning
    /// model, and every turn pays full price for a prefix it already
    /// sent — which is what polaris did until it was measured.
    #[test]
    fn the_body_asks_for_encrypted_reasoning_so_the_turn_is_cacheable() {
        let req = CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("x")],
            tools: vec![],
        };
        let body = build_body("m", &req, None);
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"]),
            "the prompt cache is being left switched off"
        );
    }

    /// When there isn't a single tool, `tools` isn't sent at all. Sending
    /// an empty array could be taken as an instruction to "use no tools."
    #[test]
    fn an_empty_tool_list_is_omitted_rather_than_sent_empty() {
        let req = CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("x")],
            tools: vec![],
        };
        let body = build_body("m", &req, None);
        assert!(body.get("tools").is_none(), "an empty tools is being sent");
    }

    /// When `effort` is passed, it rides as `reasoning.effort`. The
    /// counterpart to the fact that the `reasoning` key itself is absent
    /// when it isn't passed. With only one half of this pair, either an
    /// implementation that always emits reasoning, or one that always
    /// omits it, would pass.
    #[test]
    fn an_effort_becomes_the_reasoning_field() {
        let req = CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("x")],
            tools: vec![],
        };
        let body = build_body("m", &req, Some("xhigh"));
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct Tokens {
        calls: AtomicUsize,
        refreshes: AtomicUsize,
    }

    impl Tokens {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                refreshes: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::TokenSource for Tokens {
        async fn token(&self) -> Result<crate::Token, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::Token {
                access_token: "first".into(),
                account_id: "acct-1".into(),
                effort: None,
            })
        }
        async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(crate::Token {
                access_token: "second".into(),
                account_id: "acct-1".into(),
                effort: None,
            })
        }
    }

    fn req() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("go")],
            tools: vec![],
        }
    }

    fn sse_body(frames: &[Vec<u8>]) -> String {
        frames
            .iter()
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect()
    }

    /// The headers and body sent out must match the spec. If this is
    /// wrong, no matter how correctly the response gets interpreted, the
    /// server won't even talk to us.
    #[tokio::test]
    async fn the_request_carries_the_bearer_and_the_account_id() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer first"))
            .and(header("chatgpt-account-id", "acct-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame("response.output_item.done", message_item("ok")),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        let r = p.complete(req()).await.expect("should succeed");
        assert_eq!(r.text, "ok");
    }

    /// `set_effort` must override the account-plan-derived effort
    /// (`Tokens` here always returns `effort: None`, so any effort seen
    /// on the wire had to come from the override, not the token).
    #[tokio::test]
    async fn set_effort_overrides_the_plan_derived_effort() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame("response.output_item.done", message_item("ok")),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        p.set_effort(Some("xhigh"));
        p.complete(req()).await.expect("should succeed");

        let received = s.received_requests().await.expect("recorded");
        let body: Value = received[0].body_json().expect("json");
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    /// On receiving a 401, refresh and retry exactly once, and succeed.
    #[tokio::test]
    async fn a_401_is_retried_once_with_a_refreshed_token() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer first"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer second"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame(
                    "response.output_item.done",
                    message_item("succeeded on retry"),
                ),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(&s)
            .await;

        let t = Tokens::new();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone());
        let r = p.complete(req()).await.expect("should succeed on retry");
        assert_eq!(r.text, "succeeded on retry");
        assert_eq!(
            t.refreshes.load(Ordering::SeqCst),
            1,
            "the number of refreshes isn't 1"
        );
    }

    /// If 401 happens twice in a row, give up. Never retry indefinitely.
    /// The kind is Auth, not Http.
    #[tokio::test]
    async fn a_second_401_gives_up_as_an_auth_error() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;

        let t = Tokens::new();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone());
        let err = p.complete(req()).await.expect_err("should fail");
        assert!(matches!(err, ProviderError::Auth(_)), "not Auth: {err:?}");
        assert_eq!(
            t.refreshes.load(Ordering::SeqCst),
            1,
            "the retry didn't stop at 1"
        );
    }

    /// What the test above confirms is only that "the result is Auth,"
    /// not that "it finishes quickly." A mutation that degrades
    /// `complete`'s retry into a `loop` still compiles and would just
    /// hang the test above forever, never showing up as a red X. Here we
    /// bound it by time and additionally count how many requests the
    /// server actually received, to detect a degradation into a loop both
    /// fast and reliably. If it were looping, more than 2 requests should
    /// arrive within 500ms.
    #[tokio::test]
    async fn a_second_401_stops_retrying_within_a_time_bound() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;

        let t = Tokens::new();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone());

        let bound = Duration::from_millis(500);
        let outcome = tokio::time::timeout(bound, p.complete(req())).await;

        // wiremock has request recording on by default. Even if the
        // timeout fired, the count received up to that point is still
        // meaningful (if it were looping, it should exceed 2).
        let received = s
            .received_requests()
            .await
            .expect("request recording should be on by default")
            .len();

        let mut failures = Vec::new();
        if outcome.is_err() {
            failures.push(format!(
                "did not finish within {bound:?} ({received} received so far). the retry may be looping"
            ));
        }
        if received != 2 {
            failures.push(format!(
                "did not stop at exactly 2 (initial + 1 retry) ({received} received)"
            ));
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    /// A 500 is not Auth. Keep the retry entry point exclusive to 401. If
    /// this breaks down, every transient server failure would call
    /// `refreshed()` and burn a token.
    #[tokio::test]
    async fn a_500_response_does_not_trigger_the_refresh_and_retry_path() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&s)
            .await;

        let t = Tokens::new();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone());
        let err = p.complete(req()).await.expect_err("should fail");
        assert!(matches!(err, ProviderError::Http(_)), "not Http: {err:?}");
        assert_eq!(
            t.refreshes.load(Ordering::SeqCst),
            0,
            "a refresh (retry) happened even though it was a 500"
        );
    }

    /// A 429 includes the reset information in its wording. A rejection
    /// you can't act on just makes the same failure repeat.
    #[tokio::test]
    async fn a_429_surfaces_the_retry_hint() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "37")
                    .set_body_string("rate limited"),
            )
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        let err = p.complete(req()).await.expect_err("should fail");
        let ProviderError::Http(msg) = err else {
            panic!("not Http: {err:?}");
        };
        assert!(
            msg.contains("37"),
            "retry-after missing from message: {msg}"
        );
    }

    /// A stream that was cut off without ever seeing completion is a
    /// failure. HTTP is 200, so letting this through would return an
    /// empty final answer.
    #[tokio::test]
    async fn a_truncated_stream_is_an_error_even_on_200() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                "response.output_item.done",
                message_item("cut off midway"),
            )])))
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        let err = p.complete(req()).await.expect_err("should fail");
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "not Decode: {err:?}"
        );
    }

    /// The moment a token can't be obtained, it's Auth. Never reaches the
    /// network.
    #[tokio::test]
    async fn a_token_source_failure_is_an_auth_error() {
        struct NoTokens;
        #[async_trait::async_trait]
        impl crate::TokenSource for NoTokens {
            async fn token(&self) -> Result<crate::Token, ProviderError> {
                Err(ProviderError::Auth("not logged in".into()))
            }
            async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
                Err(ProviderError::Auth("not logged in".into()))
            }
        }

        let p = CodexProvider::new(
            "http://127.0.0.1:1/unreachable".into(),
            "m".into(),
            Arc::new(NoTokens),
        );
        let err = p.complete(req()).await.expect_err("should fail");
        assert!(matches!(err, ProviderError::Auth(_)), "not Auth: {err:?}");
    }
}
