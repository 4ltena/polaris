//! OpenAI-compatible chat completions. Swap out `base_url` and you can hit
//! compatible endpoints too.

use std::time::Duration;

use serde_json::Value;

use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError, Role, ToolCall};
use polaris_tools::ToolSpec;

/// The cap on connection establishment (TCP/TLS handshake). If the peer
/// doesn't even respond, it's fine to give up sooner than this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The idle (read) timeout. `reqwest::Client::new()` waits indefinitely by
/// default, so if the peer accepts the connection and then just hangs, a
/// one-shot headless binary would stall forever without even the help of
/// `--max-turns` — a cap against that is necessary.
///
/// This used to have a 120-second total-time cap on the "whole request"
/// (`ClientBuilder::timeout`), but that was the wrong tool. This client is
/// non-streaming, and the default model is a reasoning model, so it's
/// perfectly normal for a single healthy completion to take longer than
/// 2 minutes. A total-time cap kills that healthy response without a
/// retry, and to the user it looks like "it timed out even though nothing
/// was broken."
///
/// What's actually needed isn't a total-time cap but idle detection, and
/// `reqwest` 0.12 has `ClientBuilder::read_timeout` (the version resolved
/// in `Cargo.lock` is 0.12.28, usable without a feature gate). Note,
/// though, that for a non-streaming response it doesn't behave the way
/// the naive intuition of "gets reset on every read" would suggest:
/// `reqwest`'s implementation (`PendingRequest::poll` in
/// `async_impl/client.rs`) arms a single-shot sleep that isn't reset until
/// the response headers arrive, and only the body read *after* the
/// headers arrive gets reset on every read (`ReadTimeoutBody` in
/// `async_impl/body.rs`). This client's response doesn't stream a single
/// byte until the model has finished generating, so the former dominates
/// — meaning this value itself needs to be set comfortably larger than
/// "how long it's normal to wait for generation," not "wait forever as
/// long as reads keep coming." That said, during the later body-transfer
/// phase (when a large response arrives in pieces), the per-read reset
/// does kick in, which errs safer than a total-time cap.
///
/// 300 seconds was chosen as a value that leaves ample headroom (2.5x
/// 2 minutes) for a healthy reasoning turn that can clearly exceed
/// 2 minutes, while still detecting a dead connection within 5 minutes
/// rather than never.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    // `RwLock`, not a plain `String`/`Option<String>` — `set_model`/
    // `set_effort` take `&self` so they can be called through the same
    // shared `&dyn Provider` the rest of the harness already holds (see
    // the trait's docs).
    model: std::sync::RwLock<String>,
    effort: std::sync::RwLock<Option<String>>,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// # Errors
    ///
    /// Returned when `reqwest::Client` cannot be constructed, e.g. TLS
    /// backend initialization failure. `expect`-ing here and taking down
    /// the whole startup process would create an asymmetry where every
    /// other startup failure in the harness (misconfiguration, etc.)
    /// comes back as a clean one-line `stderr` plus `ExitCode::FAILURE`,
    /// while this one spot alone panics. The caller (`main.rs`) handles
    /// this through the same path as other startup failures.
    pub fn new(base_url: String, api_key: String, model: String) -> Result<Self, ProviderError> {
        Self::with_timeouts(base_url, api_key, model, CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// Constructs with explicit timeout values. The path used to inject a
    /// short timeout so hang behavior can be tested. Normal callers use
    /// [`Self::new`].
    fn with_timeouts(
        base_url: String,
        api_key: String,
        model: String,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| ProviderError::Http(format!("could not build HTTP client: {e}")))?;
        Ok(Self {
            base_url,
            api_key,
            model: std::sync::RwLock::new(model),
            effort: std::sync::RwLock::new(None),
            client,
        })
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// The sole place that converts a `ToolSpec` into the wire shape
/// (`{"type":"function","function":{...}}`).
///
/// Budget accounting (`polaris_core::budget::always_on_tokens`) also
/// counts this function's output. Budget accounting used to count
/// `ToolSpec`'s `Serialize` implementation directly, with this function
/// independently building the same shape elsewhere. Having two places
/// independently build what was supposed to be "the same" wire shape is
/// what caused the bytes actually sent and the bytes actually measured to
/// drift apart, so the shape now has a single source of truth here.
pub fn tool_wire_shape(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": req.system,
        })];
        for m in &req.messages {
            let mut entry = serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            });
            // If an assistant turn called a tool, its `tool_calls` ride
            // along on this same message when sent back. The API matches
            // these against the `tool_call_id` of the `role: "tool"`
            // message that follows. `arguments` is parsed into a JSON
            // value once on receipt, and symmetrically turned back into a
            // JSON string on send (on the wire, both are strings).
            if !m.tool_calls.is_empty() {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": c.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                entry["tool_calls"] = Value::Array(calls);
            }
            if let Some(id) = &m.tool_call_id {
                entry["tool_call_id"] = Value::String(id.clone());
            }
            messages.push(entry);
        }

        let tools = tool_wire_shape(&req.tools);

        let model = self.model.read().expect("model lock poisoned").clone();
        let effort = self.effort.read().expect("effort lock poisoned").clone();
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            // See `crate::cache_key`. Sent here for the same reason as
            // in `codex::build_body`: a stable prefix is only reused if
            // the backend is pointed at the shard holding it.
            "prompt_cache_key": crate::cache_key(&model, effort.as_deref(), &req),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(effort) = effort.as_deref() {
            body["reasoning_effort"] = Value::String(effort.to_string());
        }

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                // `reqwest::Error`'s `Display` doesn't say whether it was
                // a timeout (it only prints something like `error sending
                // request for url (...)`). Without checking `is_timeout()`
                // and spelling out here that this failed because of the
                // idle timeout, the user can't tell a dropped connection
                // apart from a timeout.
                if e.is_timeout() {
                    ProviderError::Http(format!(
                        "the request timed out (no response for a while): {e}"
                    ))
                } else {
                    ProviderError::Http(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(ProviderError::Http(format!("status {}", resp.status())));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        // If `choices` is missing/empty, or choices[0].message is missing,
        // the response couldn't be interpreted. Without turning this into
        // Decode here, it becomes a "looks like a normal completion"
        // empty response (text="" / tool_calls=[]), and the agent loop
        // above misreads "no tool call" as "done" and silently breaks.
        let msg = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("message"))
            .ok_or_else(|| {
                ProviderError::Decode("choices is empty, or message is missing".into())
            })?;

        // Only turn this into Decode when both content and tool_calls are
        // "effectively absent." "Absent" has to be defined symmetrically
        // for both sides — if only one side is lenient, the same silent
        // empty success slips out through that lenient gate.
        //
        // content is "absent" when the field itself is missing, or it's
        // JSON null. There's no meaningful distinction to draw here on
        // the wire between missing and null. Meanwhile, when content is
        // an empty string, it "is present" — that's treated as a normal
        // response where the model simply said nothing (never treat empty
        // as equivalent to absent).
        //
        // tool_calls is "absent" when the field itself is missing, or
        // it's JSON null, or it's an empty array. `{"content": null,
        // "tool_calls": null}` and `{"content": null, "tool_calls": []}`
        // are semantically identical to the missing-key case (no call was
        // ever requested), and without treating them the same, the
        // "looks like a normal completion" empty response of text="" /
        // tool_calls=[] slips past this guard (the loop misreads it as
        // "no tool call = done" and returns an empty string as the final
        // answer, silently breaking).
        //
        // The combination of `content: null` with a non-empty tool_calls
        // (the normal shape of a tool-only turn) does not trip this guard.
        let content_field = msg.get("content");
        let tool_calls_field = msg.get("tool_calls");
        let content_is_absent = content_field.is_none_or(|c| c.is_null());
        let tool_calls_are_absent = tool_calls_field
            .is_none_or(|c| c.is_null() || c.as_array().is_some_and(|a| a.is_empty()));
        if content_is_absent && tool_calls_are_absent {
            return Err(ProviderError::Decode(
                "message has neither content nor tool_calls".into(),
            ));
        }
        let text = content_field
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(calls) = tool_calls_field.and_then(|tc| tc.as_array()) {
            for c in calls {
                // If function.name is missing/empty, it reaches the
                // dispatcher looking like "an unknown tool with an empty
                // name," and the decode failure ends up looking like a
                // tool-selection problem. Stop it here and report it at
                // the source instead.
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ProviderError::Decode(
                            "tool_calls[].function.name is missing or empty".into(),
                        )
                    })?;

                // id gets the same missing/empty treatment as Decode, for
                // the same reason. It's a field with the same shape as
                // name, at the same cost, so it's kept consistent.
                // That said, exactly how an empty id would actually break
                // things downstream (matching against tool results)
                // hasn't been confirmed.
                let id = c
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ProviderError::Decode("tool_calls[].id is missing or empty".into())
                    })?;

                let raw = c["function"]["arguments"].as_str().unwrap_or("{}");
                let arguments: Value =
                    serde_json::from_str(raw).map_err(|e| ProviderError::Decode(e.to_string()))?;
                tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
        }

        let usage = v.get("usage").and_then(|u| {
            let input_tokens = u.get("prompt_tokens")?.as_u64()? as u32;
            let output_tokens = u.get("completion_tokens")?.as_u64()? as u32;
            let total_tokens = u.get("total_tokens")?.as_u64()? as u32;
            let cached_tokens = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            Some(crate::Usage {
                input_tokens,
                output_tokens,
                total_tokens,
                cached_tokens,
            })
        });

        Ok(CompletionResponse {
            text,
            tool_calls,
            reasoning: Vec::new(),
            usage,
        })
    }

    fn set_model(&self, model: &str) {
        *self.model.write().expect("model lock poisoned") = model.to_string();
    }

    fn set_effort(&self, effort: Option<&str>) {
        *self.effort.write().expect("effort lock poisoned") = effort.map(str::to_string);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolCall};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_tool_call_from_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "function": { "name": "read", "arguments": "{\"path\":\"a.rs\"}" }
                        }]
                    }
                }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
            .expect("client should be constructible");
        let res = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![Message::user("go")],
                tools: vec![],
            })
            .await
            .expect("should succeed");

        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "read");
        assert_eq!(res.tool_calls[0].arguments["path"], "a.rs");
    }

    #[tokio::test]
    async fn surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
            .expect("client should be constructible");
        let err = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![],
                tools: vec![],
            })
            .await
            .expect_err("should be an error");
        assert!(matches!(err, ProviderError::Http(_)));
    }

    async fn complete_against(
        body: serde_json::Value,
    ) -> Result<CompletionResponse, ProviderError> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
            .expect("client should be constructible");
        p.complete(CompletionRequest {
            system: "s".into(),
            messages: vec![],
            tools: vec![],
        })
        .await
    }

    #[tokio::test]
    async fn errors_when_choices_is_missing() {
        let err = complete_against(serde_json::json!({}))
            .await
            .expect_err("should be an error since choices is missing");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_choices_is_empty() {
        let err = complete_against(serde_json::json!({ "choices": [] }))
            .await
            .expect_err("should be an error since choices is empty");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_message_has_neither_content_nor_tool_calls() {
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": {} }]
        }))
        .await
        .expect_err("should be an error since neither content nor tool_calls is present");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_content_is_null_and_tool_calls_missing() {
        // content: null is "present" in JSON terms, but if tool_calls is
        // also missing, the response effectively has no body at all, and
        // this used to silently become Ok as "". The loop reads that as
        // "no tool call = done," so this ends up as the wrong result of
        // "succeeded with an empty answer."
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": null } }]
        }))
        .await
        .expect_err("should be an error since content is null and tool_calls is also missing");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_content_is_null_and_tool_calls_is_null() {
        // tool_calls: null is semantically identical to "the field is
        // missing" (no call was ever requested), and without treating
        // them the same, the "looks like a normal completion" empty
        // response of text="" / tool_calls=[] slips past the guard.
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": null, "tool_calls": null } }]
        }))
        .await
        .expect_err("should be an error since both content and tool_calls are null");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_content_is_null_and_tool_calls_is_empty_array() {
        // tool_calls: [] is likewise "no calls," and unless it's treated
        // the same as missing/null, it becomes the same escape hatch.
        let err = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": null, "tool_calls": [] } }]
        }))
        .await
        .expect_err("should be an error since content is null and tool_calls is an empty array");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn empty_string_content_is_a_real_response_not_an_error() {
        let res = complete_against(serde_json::json!({
            "choices": [{ "message": { "content": "" } }]
        }))
        .await
        .expect("an empty string content should still be treated as a normal response");
        assert_eq!(res.text, "");
        assert_eq!(res.tool_calls.len(), 0);
    }

    #[tokio::test]
    async fn errors_when_tool_call_missing_function_name() {
        let err = complete_against(serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "arguments": "{}" }
                    }]
                }
            }]
        }))
        .await
        .expect_err("should be an error since function.name is missing");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_tool_call_has_empty_function_name() {
        let err = complete_against(serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "", "arguments": "{}" }
                    }]
                }
            }]
        }))
        .await
        .expect_err("should be an error since function.name is an empty string");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn errors_when_tool_call_missing_id() {
        let err = complete_against(serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "function": { "name": "read", "arguments": "{}" }
                    }]
                }
            }]
        }))
        .await
        .expect_err("should be an error since id is missing");
        assert!(matches!(err, ProviderError::Decode(_)));
    }

    #[tokio::test]
    async fn read_timeout_bounds_a_stalled_response_and_says_so() {
        // `reqwest::Client::new()` waits indefinitely. Reproduce the case
        // where the peer accepts the connection but never returns a
        // response, using a mock's delayed response, and confirm that we
        // reliably get back an error within the configured read_timeout,
        // and that the error's wording makes clear it was a timeout.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(300))
                    .set_body_json(serde_json::json!({
                        "choices": [{ "message": { "content": "slow" } }]
                    })),
            )
            .mount(&server)
            .await;

        let p = OpenAiProvider::with_timeouts(
            server.uri(),
            "k".into(),
            "m".into(),
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(50),
        )
        .expect("client should be constructible");

        let started = std::time::Instant::now();
        let err = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![],
                tools: vec![],
            })
            .await
            .expect_err("should be an error from the timeout");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the timeout didn't take effect. actual elapsed: {:?}",
            started.elapsed()
        );
        let ProviderError::Http(msg) = err else {
            panic!("should be an Http error");
        };
        assert!(
            msg.contains("timed out"),
            "wording identifying this as a timeout is missing: {msg}"
        );
    }

    #[tokio::test]
    async fn sends_tool_definitions_in_the_shape_tool_wire_shape_produces() {
        // Confirm, from the raw body the mock actually received, that the
        // shape budget accounting counts (`tool_wire_shape`) and the shape
        // that actually rides the wire come from the same function. A
        // test that only looks at types can't detect the two
        // independently reimplementing the same shape and drifting apart.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let specs = polaris_tools::all_specs();
        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
            .expect("client should be constructible");
        p.complete(CompletionRequest {
            system: "s".into(),
            messages: vec![],
            tools: specs.clone(),
        })
        .await
        .expect("should succeed");

        let received = server
            .received_requests()
            .await
            .expect("request should have been recorded");
        let body: Value = received[0].body_json().expect("should be readable as JSON");

        let expected = Value::Array(tool_wire_shape(&specs));
        assert_eq!(
            body["tools"], expected,
            "the tool definitions sent don't match tool_wire_shape's output"
        );
    }

    #[tokio::test]
    async fn a_message_with_reasoning_is_sent_unchanged_since_chat_completions_has_no_such_concept()
    {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
            .expect("client should be constructible");
        let with_reasoning = Message::assistant("yes").with_reasoning(vec![crate::ReasoningItem {
            id: "r1".into(),
            encrypted_content: "opaque".into(),
        }]);
        let without_reasoning = Message::assistant("yes");

        p.complete(CompletionRequest {
            system: "s".into(),
            messages: vec![with_reasoning],
            tools: vec![],
        })
        .await
        .expect("should succeed");
        p.complete(CompletionRequest {
            system: "s".into(),
            messages: vec![without_reasoning],
            tools: vec![],
        })
        .await
        .expect("should succeed");

        let received = server.received_requests().await.expect("recorded");
        let with_body: Value = received[0].body_json().expect("json");
        let without_body: Value = received[1].body_json().expect("json");
        assert_eq!(
            with_body["messages"], without_body["messages"],
            "a Message's reasoning field must not change the Chat Completions request body"
        );
    }

    #[tokio::test]
    async fn set_model_changes_the_model_sent_on_the_next_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "gpt-5.4".into())
            .expect("client should be constructible");
        p.complete(CompletionRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
        })
        .await
        .expect("should succeed");

        p.set_model("gpt-5.6-sol");
        p.complete(CompletionRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
        })
        .await
        .expect("should succeed");

        let received = server.received_requests().await.expect("recorded");
        assert_eq!(received.len(), 2);
        let first: Value = received[0].body_json().expect("json");
        let second: Value = received[1].body_json().expect("json");
        assert_eq!(first["model"], "gpt-5.4");
        assert_eq!(second["model"], "gpt-5.6-sol");
    }

    #[tokio::test]
    async fn set_effort_adds_reasoning_effort_to_the_body_and_none_clears_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "ok" } }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "gpt-5.4".into())
            .expect("client should be constructible");
        fn empty_request() -> CompletionRequest {
            CompletionRequest {
                system: String::new(),
                messages: vec![],
                tools: vec![],
            }
        }

        p.complete(empty_request()).await.expect("should succeed");

        p.set_effort(Some("high"));
        p.complete(empty_request()).await.expect("should succeed");

        p.set_effort(None);
        p.complete(empty_request()).await.expect("should succeed");

        let received = server.received_requests().await.expect("recorded");
        assert_eq!(received.len(), 3);
        let bodies: Vec<Value> = received.iter().map(|r| r.body_json().unwrap()).collect();
        assert!(bodies[0].get("reasoning_effort").is_none());
        assert_eq!(bodies[1]["reasoning_effort"], "high");
        assert!(bodies[2].get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn serializes_tool_round_trip_to_the_wire_shape_openai_requires() {
        // A test that only looks at types misses an actual drift in the
        // wire format. Here we directly verify the raw request body
        // wiremock received, confirming: (1) an assistant message
        // carrying tool_calls stays in the history as-is, (2) arguments
        // is sent as a JSON string rather than a JSON object, (3) the
        // tool result that follows carries tool_call_id, and (4) an
        // ordinary user message carries neither tool_calls nor
        // tool_call_id.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "it was 1 line" } }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
            .expect("client should be constructible");
        let history = vec![
            Message::user("how many lines is a.txt?"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "a.txt" }),
                }],
            ),
            Message::tool_result("c1", "1: hello"),
        ];

        p.complete(CompletionRequest {
            system: "s".into(),
            messages: history,
            tools: vec![],
        })
        .await
        .expect("should succeed");

        let received = server
            .received_requests()
            .await
            .expect("request should have been recorded");
        assert_eq!(received.len(), 1);
        let body: Value = received[0].body_json().expect("should be readable as JSON");
        let messages = body["messages"].as_array().expect("messages is missing");

        // 0: system, 1: user, 2: assistant(tool_calls), 3: tool
        let user = &messages[1];
        assert_eq!(user["role"], "user");
        assert!(
            user.get("tool_calls").is_none(),
            "tool_calls is riding on an ordinary user message: {user:?}"
        );
        assert!(
            user.get("tool_call_id").is_none(),
            "tool_call_id is riding on an ordinary user message: {user:?}"
        );

        let assistant = &messages[2];
        assert_eq!(assistant["role"], "assistant");
        let calls = assistant["tool_calls"]
            .as_array()
            .expect("assistant's tool_calls is missing");
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read");
        let arguments = &calls[0]["function"]["arguments"];
        assert!(
            arguments.is_string(),
            "arguments should be a JSON string: {arguments:?}"
        );
        let parsed: Value =
            serde_json::from_str(arguments.as_str().unwrap()).expect("should be parseable");
        assert_eq!(parsed["path"], "a.txt");

        let tool_msg = &messages[3];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "c1");
        assert_eq!(tool_msg["content"], "1: hello");
    }

    #[tokio::test]
    async fn usage_is_parsed_from_a_normal_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi", "tool_calls": null}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            })))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "key".into(), "model".into()).expect("client");
        let res = provider
            .complete(CompletionRequest {
                system: String::new(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("should succeed");

        let usage = res.usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(
            usage.cached_tokens, 0,
            "a response with no prompt_tokens_details must not fail to parse, just report 0"
        );
    }

    #[tokio::test]
    async fn cached_tokens_is_parsed_from_prompt_tokens_details() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi", "tool_calls": null}}],
                "usage": {
                    "prompt_tokens": 2006,
                    "completion_tokens": 300,
                    "total_tokens": 2306,
                    "prompt_tokens_details": {"cached_tokens": 1920}
                }
            })))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "key".into(), "model".into()).expect("client");
        let res = provider
            .complete(CompletionRequest {
                system: String::new(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("should succeed");

        let usage = res.usage.expect("usage should be present");
        assert_eq!(usage.cached_tokens, 1920);
    }

    #[tokio::test]
    async fn a_response_without_usage_yields_none_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi", "tool_calls": null}}]
            })))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "key".into(), "model".into()).expect("client");
        let res = provider
            .complete(CompletionRequest {
                system: String::new(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("a missing usage field must not fail the whole response");

        assert!(res.usage.is_none());
    }
}
