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

#[path = "cache_prefix.rs"]
mod cache_prefix;
#[path = "codex_metrics.rs"]
mod metrics;

use crate::attempts::{AttemptLedger, LogicalRequest};
use crate::cache_pacing::{Gate as PacingGate, Mode as PacingMode};
use crate::turn_affinity::{TurnAffinityMode, TurnContext, TurnIdentity};
use crate::web_search::{WebSearchConfig, parse_hosted_web_search_item, parse_url_citations};
use crate::{
    CompletionRequest, CompletionResponse, Message, Provider, ProviderError, ReasoningItem, Role,
    ToolCall, sse,
};

/// The endpoint to send requests to. `store` is never used, so this is the
/// only one ever hit.
pub const ENDPOINT_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// The default used when `POLARIS_MODEL` is omitted.
///
/// The product default. CLI configuration supplies the explicit medium
/// reasoning effort; this constant intentionally selects only the model.
pub const DEFAULT_MODEL: &str = "gpt-6-astra";

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
                // Reasoning items are replayed before the message/
                // function_call they informed, matching how the model
                // originally emitted them. Note this only preserves
                // turn-level ordering, not fine-grained interleaving:
                // polaris collapses a turn's reasoning items and tool_calls
                // into two separate flat Vecs, so a turn that actually
                // produced reasoning -> call -> reasoning -> call emits all
                // of its reasoning here first, followed by all of its
                // tool_calls below, not interleaved to match the original
                // emission. Deliberate simplification, not a bug.
                //
                // Only emitted when the turn goes on to produce a message
                // or tool_calls: a reasoning item with nothing following it
                // is a wire shape the Responses API rejects when
                // `store: false` (see `build_body`, the only mode this
                // provider ever uses).
                if !m.content.is_empty() || !m.tool_calls.is_empty() {
                    for r in &m.reasoning {
                        out.push(serde_json::json!({
                            "type": "reasoning",
                            // Upstream's `prepare_response_items_for_request`
                            // strips all item ids from every request when
                            // `store: false` (polaris's only mode) - match
                            // that instead of sending one, even though the
                            // captured `ReasoningItem` still keeps its `id`.
                            "summary": [],
                            "encrypted_content": r.encrypted_content,
                        }));
                    }
                }
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
    build_body_with_instructions(model, req, &req.system, effort)
}

/// Adds hosted web search only after the selected endpoint has been verified
/// against the complete bounded Responses contract. The disabled configuration
/// returns the historical `build_body` result unchanged.
pub fn build_body_with_web_search(
    model: &str,
    req: &CompletionRequest,
    effort: Option<&str>,
    config: WebSearchConfig,
) -> Result<Value, ProviderError> {
    let mut body = build_body(model, req, effort);
    config
        .apply_to_body(&mut body)
        .map_err(|error| ProviderError::Unsupported(error.to_string()))?;
    Ok(body)
}

fn build_body_with_instructions(
    model: &str,
    req: &CompletionRequest,
    instructions: &str,
    effort: Option<&str>,
) -> Value {
    // Use the same effective instructions for the wire body and the existing
    // cache-key function. A stable guide must therefore be reflected before
    // cache-key construction, while compact retains the old values exactly.
    let effective_req = CompletionRequest {
        system: instructions.to_string(),
        messages: Vec::new(),
        tools: req.tools.clone(),
    };
    let mut body = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": input_items(&req.messages),
        // Do not request stored response state. Send the locally assembled
        // history explicitly; store:false does not itself guarantee a stable
        // prefix or reduce billed input tokens.
        "store": false,
        "stream": true,
        // Added 2026-08-25 on the strength of a two-turn run that showed
        // `cache_write_tokens`/`cached_tokens` at 0 without this and 7,680
        // of 8,014 cached with it. A direct A/B on 2026-08-26 (build with
        // this change vs. without, same task, multiple turns) couldn't
        // reproduce that gap — both builds reached 93-97% cached_tokens by
        // the third turn regardless. Left on: it costs only a slightly
        // larger response payload, but its necessity for caching is no
        // longer something this code can claim as measured.
        "include": ["reasoning.encrypted_content"],
        // Routing hint combined with the backend's prefix hash. It neither
        // pins a machine nor guarantees a cache hit; see `crate::cache_key`.
        "prompt_cache_key": crate::cache_key(model, effort, &effective_req),
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
    reasoning: Vec<ReasoningItem>,
    hosted_web_search: Vec<crate::web_search::HostedWebSearchItem>,
    url_citations: Vec<crate::web_search::UrlCitation>,
    metrics_usage: metrics::TokenUsage,
    cancelled: bool,
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
                // Deltas and everything else are skipped. Looking only at
                // finalized items gives the same result, and doesn't drag
                // in reassembly failure as a new way to break.
                _ => {}
            }
        }
        Ok(())
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
            reasoning: self.reasoning,
            usage: self.usage,
            hosted_web_search: self.hosted_web_search,
            url_citations: self.url_citations,
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
    cache_namespace: Option<std::ffi::OsString>,
    cache_mode: Option<std::ffi::OsString>,
    cache_prefix: cache_prefix::CachePrefixProfile,
    turn_affinity: TurnAffinityMode,
    pacing: PacingGate,
    attempt_ledger: Option<AttemptLedger>,
    web_search: WebSearchConfig,
    request_caps: Option<crate::attempts::RequestCaps>,
    visible_request_byte_limit: Option<usize>,
}

impl CodexProvider {
    pub fn new(
        base: String,
        model: String,
        tokens: Arc<dyn crate::TokenSource>,
    ) -> Result<Self, ProviderError> {
        Self::with_idle_timeout(base, model, tokens, DEFAULT_IDLE_TIMEOUT)
    }

    pub fn with_idle_timeout(
        base: String,
        model: String,
        tokens: Arc<dyn crate::TokenSource>,
        idle: Duration,
    ) -> Result<Self, ProviderError> {
        Self::with_idle_timeout_and_affinity(
            base,
            model,
            tokens,
            idle,
            TurnAffinityMode::from_env()?,
        )
    }

    fn with_idle_timeout_and_affinity(
        base: String,
        model: String,
        tokens: Arc<dyn crate::TokenSource>,
        idle: Duration,
        turn_affinity: TurnAffinityMode,
    ) -> Result<Self, ProviderError> {
        let client = polaris_http::client_builder()
            .map_err(|e| ProviderError::Http(format!("could not configure HTTP client: {e}")))?
            .build()
            .map_err(|e| ProviderError::Http(format!("could not build HTTP client: {e}")))?;
        Ok(Self {
            base,
            model: std::sync::RwLock::new(model),
            effort_override: std::sync::RwLock::new(None),
            tokens,
            client,
            idle,
            cache_namespace: std::env::var_os("POLARIS_CACHE_NAMESPACE"),
            cache_mode: std::env::var_os("POLARIS_CACHE_MODE"),
            cache_prefix: cache_prefix::CachePrefixProfile::from_env()?,
            turn_affinity,
            pacing: PacingGate::new(PacingMode::from_env()?),
            attempt_ledger: None,
            web_search: WebSearchConfig::default(),
            request_caps: None,
            visible_request_byte_limit: None,
        })
    }

    /// Attaches physical HTTP-attempt observation to this transport.
    /// Without this hook, request bytes and behavior remain unchanged.
    pub fn with_attempt_ledger(mut self, ledger: AttemptLedger) -> Self {
        self.attempt_ledger = Some(ledger);
        self
    }

    pub fn with_request_caps(mut self, caps: crate::attempts::RequestCaps) -> Self {
        self.request_caps = Some(caps);
        self
    }

    /// Rejects a final serialized request body above this UTF-8 byte limit.
    /// This bounds visible request bytes only; it does not claim a token cap.
    pub fn with_visible_request_byte_limit(mut self, limit: usize) -> Self {
        self.visible_request_byte_limit = Some(limit);
        self
    }

    fn validate_optional_contracts(&self) -> Result<(), ProviderError> {
        if let Some(caps) = self.request_caps {
            caps.validate()?;
            if self.attempt_ledger.is_none() {
                return Err(ProviderError::Budget(
                    "bounded requests need an attempt ledger".into(),
                ));
            }
            if self.web_search.policy != crate::web_search::WebSearchPolicy::Disabled
                && caps.max_hosted_actions == 0
            {
                return Err(ProviderError::Budget(
                    "hosted actions need a reservation".into(),
                ));
            }
        }
        self.web_search
            .apply_to_body(&mut serde_json::json!({}))
            .map_err(|e| ProviderError::Unsupported(e.to_string()))
    }

    /// Configures hosted web search without changing `CompletionRequest`.
    pub fn with_web_search_config(mut self, config: WebSearchConfig) -> Self {
        self.web_search = config;
        self
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
        turn: Option<&TurnContext>,
        logical: Option<&LogicalRequest>,
        retry_reason: Option<&str>,
    ) -> Result<CompletionResponse, ProviderError> {
        let model = self.model.read().expect("model lock poisoned").clone();
        let override_effort = self
            .effort_override
            .read()
            .expect("effort lock poisoned")
            .clone();
        let effort = override_effort.as_deref().or(token.effort.as_deref());
        let instructions = self.cache_prefix.instructions(&model, req);
        let mut body = build_body_with_instructions(&model, req, &instructions, effort);
        self.web_search
            .apply_to_body(&mut body)
            .map_err(|error| ProviderError::Unsupported(error.to_string()))?;
        metrics::apply_namespace(&mut body, self.cache_namespace.as_deref())?;
        metrics::apply_cache_mode(&mut body, self.cache_mode.as_deref())?;
        let reservation = self
            .request_caps
            .map(|caps| caps.apply(&mut body))
            .transpose()?
            .unwrap_or_default();
        if let Some(limit) = self.visible_request_byte_limit {
            let bytes = serde_json::to_vec(&body)
                .map_err(|error| ProviderError::Budget(error.to_string()))?
                .len();
            if bytes > limit {
                return Err(ProviderError::Budget(
                    "visible request exceeds UTF-8 byte limit".into(),
                ));
            }
        }
        let prefix_diagnostics = self.cache_prefix.diagnostics(&model, req);
        let mut metric = metrics::RequestMetric::from_env(&body, prefix_diagnostics)?;
        let identity = TurnIdentity {
            origin: self.base.clone(),
            account_id: token.account_id.clone(),
            model,
            effort: effort.map(str::to_string),
            cache_key: body["prompt_cache_key"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        };
        let sent = match (self.turn_affinity, turn) {
            (TurnAffinityMode::On, Some(turn)) => turn.take_matching(&identity),
            _ => None,
        };
        if let Some(metric) = metric.as_mut() {
            metric.defer_dispatch();
            metric.turn_affinity(self.turn_affinity, turn.is_some(), sent.is_some(), false);
        }
        let mut attempt_guard = None;
        let result = async {
            let mut request = self
                .client
                .post(format!("{}/responses", self.base))
                .bearer_auth(&token.access_token)
                .header("chatgpt-account-id", &token.account_id)
                .header("accept", "text/event-stream")
                .json(&body);
            if let Some(state) = sent.as_ref() {
                request = request.header("x-codex-turn-state", state.clone());
            }
            // Prepare the body, request and diagnostic file before waiting.
            // The gate captures one dispatch instant for both pacing and metrics.
            let dispatch = self.pacing.dispatch().await;
            if let Some(metric) = metric.as_mut() {
                metric.start_dispatch(&dispatch);
                metric.cache_pacing(
                    self.pacing.mode(),
                    self.pacing.interval(),
                    dispatch.wait,
                    dispatch.offset,
                );
            }
            // Start only after pacing grants this dispatch and immediately before
            // reqwest polls the HTTP request. Drop records cancellation from here.
            attempt_guard = match (&self.attempt_ledger, logical) {
                (Some(ledger), Some(logical)) => Some(
                    ledger
                        .begin_attempt(logical, &identity.model, retry_reason, reservation)
                        .map_err(|error| ProviderError::Budget(error.to_string()))?,
                ),
                _ => None,
            };
            let resp = request.send().await.map_err(|e| {
                if let Some(metric) = metric.as_mut() {
                    metric.transport_cause = Some(metrics::transport_cause(&e));
                }
                ProviderError::Http(e.to_string())
            })?;

            let status = resp.status();
            if let Some(metric) = metric.as_mut() {
                metric.http_status = Some(status.as_u16());
            }
            let received = resp
                .headers()
                .get("x-codex-turn-state")
                .filter(|value| !value.as_bytes().is_empty())
                .cloned();
            if let Some(metric) = metric.as_mut() {
                metric.turn_affinity(
                    self.turn_affinity,
                    turn.is_some(),
                    sent.is_some(),
                    received.is_some(),
                );
            }
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
                        if let Some(metric) = metric.as_mut() {
                            metric.transport_cause = Some("idle_timeout");
                        }
                        return Err(ProviderError::Http(format!(
                            "no response arrived for {} seconds",
                            self.idle.as_secs()
                        )));
                    }
                    Ok(None) => break,
                    Ok(Some(chunk)) => {
                        let bytes = chunk.map_err(|e| {
                            if let Some(metric) = metric.as_mut() {
                                metric.transport_cause = Some(metrics::transport_cause(&e));
                            }
                            ProviderError::Http(e.to_string())
                        })?;
                        let pushed = folder.push(&bytes);
                        if let Some(metric) = metric.as_mut() {
                            metric.usage = folder.metrics_usage;
                            metric.server_cancelled = folder.cancelled;
                        }
                        pushed?;
                    }
                }
            }
            let completed = folder.finish()?;
            // A header only becomes reusable after the SSE itself confirms a
            // successful response. It remains opaque and is never logged.
            if self.turn_affinity == TurnAffinityMode::On
                && let (Some(turn), Some(state)) = (turn, received)
            {
                turn.store_first(identity, state);
            }
            Ok(completed)
        }
        .await;
        if let Some(guard) = attempt_guard.as_mut() {
            guard.finish(&result);
        }
        if let Some(metric) = metric.as_mut() {
            metric.finish(&result);
        }
        result
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.validate_optional_contracts()?;
        let logical = self
            .attempt_ledger
            .as_ref()
            .map(AttemptLedger::begin_logical);
        let token = self.tokens.token().await?;
        match self
            .attempt(&token, &req, None, logical.as_ref(), None)
            .await
        {
            Err(ProviderError::Auth(_)) => {
                // Exactly once. Never retry indefinitely.
                let token = self.tokens.refreshed().await?;
                self.attempt(
                    &token,
                    &req,
                    None,
                    logical.as_ref(),
                    Some("401 unauthorized"),
                )
                .await
                .map_err(|e| match e {
                    ProviderError::Auth(_) => ProviderError::Auth(
                        "auth was still refused after refreshing. redo `polaris login`".into(),
                    ),
                    other => other,
                })
            }
            other => other,
        }
    }

    async fn complete_in_turn(
        &self,
        req: CompletionRequest,
        turn: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        self.validate_optional_contracts()?;
        let logical = self
            .attempt_ledger
            .as_ref()
            .map(AttemptLedger::begin_logical);
        let token = self.tokens.token().await?;
        match self
            .attempt(&token, &req, Some(turn), logical.as_ref(), None)
            .await
        {
            Err(ProviderError::Auth(_)) => {
                let token = self.tokens.refreshed().await?;
                self.attempt(
                    &token,
                    &req,
                    Some(turn),
                    logical.as_ref(),
                    Some("401 unauthorized"),
                )
                .await
                .map_err(|e| match e {
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

    #[test]
    fn default_model_is_gpt_6_astra() {
        assert_eq!(DEFAULT_MODEL, "gpt-6-astra");
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

    fn reasoning_item(id: &str, encrypted_content: &str) -> Value {
        serde_json::json!({
            "item": {
                "type": "reasoning",
                "id": id,
                "summary": [],
                "encrypted_content": encrypted_content,
            }
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

    #[test]
    fn a_reasoning_item_is_captured_with_its_encrypted_content() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            reasoning_item("r1", "opaque-blob"),
        ))
        .expect("push should succeed");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should be complete");
        assert_eq!(r.reasoning.len(), 1);
        assert_eq!(r.reasoning[0].id, "r1");
        assert_eq!(r.reasoning[0].encrypted_content, "opaque-blob");
    }

    #[test]
    fn a_reasoning_item_without_encrypted_content_is_skipped() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            serde_json::json!({ "item": { "type": "reasoning", "id": "r1", "summary": [] } }),
        ))
        .expect("push should succeed");
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");
        let r = f.finish().expect("should be complete");
        assert!(
            r.reasoning.is_empty(),
            "a reasoning item with no encrypted_content must not be kept"
        );
    }

    /// An empty string must be treated the same as absent, matching the
    /// sibling `function_call` arm's `.filter(|s| !s.is_empty())` pattern.
    /// Covers id-empty, encrypted_content-empty, and both-empty.
    #[test]
    fn a_reasoning_item_with_an_empty_id_or_encrypted_content_is_skipped() {
        for (id, encrypted_content) in [("", "blob"), ("r1", ""), ("", "")] {
            let mut f = Folder::new();
            f.push(&frame(
                "response.output_item.done",
                reasoning_item(id, encrypted_content),
            ))
            .expect("push should succeed");
            f.push(&frame("response.completed", serde_json::json!({})))
                .expect("push should succeed");
            let r = f.finish().expect("should be complete");
            assert!(
                r.reasoning.is_empty(),
                "id={id:?} encrypted_content={encrypted_content:?} should have been skipped"
            );
        }
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
    fn disabled_web_search_keeps_the_existing_body_exact() {
        let request = req();
        assert_eq!(
            build_body("gpt-6-astra", &request, Some("medium")),
            build_body_with_web_search(
                "gpt-6-astra",
                &request,
                Some("medium"),
                WebSearchConfig::disabled(),
            )
            .expect("disabled is a no-op"),
        );
    }

    #[test]
    fn live_web_search_is_rejected_before_a_request_body_can_be_used() {
        let error = build_body_with_web_search(
            "gpt-6-astra",
            &req(),
            Some("medium"),
            WebSearchConfig::live(
                crate::web_search::WebSearchRequestCaps::new(2, 4096).expect("caps"),
            ),
        )
        .expect_err("the Codex endpoint is unverified");
        assert!(matches!(error, ProviderError::Unsupported(_)));
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
    fn a_turn_with_reasoning_emits_it_before_the_message_and_calls() {
        let items = input_items(&[Message::assistant_with_tool_calls(
            "I'll read it",
            vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
        )
        .with_reasoning(vec![crate::ReasoningItem {
            id: "r1".into(),
            encrypted_content: "opaque".into(),
        }])]);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "reasoning");
        assert!(
            items[0].get("id").is_none(),
            "id must be stripped from the replayed reasoning item, matching upstream's \
             behavior when store: false"
        );
        assert_eq!(items[0]["encrypted_content"], "opaque");
        assert_eq!(items[0]["summary"], serde_json::json!([]));
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[2]["type"], "function_call");
    }

    #[test]
    fn a_turn_without_reasoning_emits_no_reasoning_item() {
        let items = input_items(&[Message::assistant("yes")]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
    }

    /// `agent::run_loop`'s no-tool-calls branch can push a `Message` with
    /// non-empty `reasoning` but empty content and empty `tool_calls` (a
    /// text-free final turn). Emitting the reasoning item alone would put
    /// a reasoning item on the wire with nothing following it - a shape
    /// the Responses API rejects when `store: false`. It must be dropped
    /// entirely, not just left orphaned.
    #[test]
    fn a_turn_with_reasoning_but_no_content_or_tool_calls_emits_nothing() {
        let items = input_items(&[Message::assistant("").with_reasoning(vec![
            crate::ReasoningItem {
                id: "r1".into(),
                encrypted_content: "opaque".into(),
            },
        ])]);
        assert!(
            items.is_empty(),
            "an orphaned reasoning item was emitted with nothing following it: {items:?}"
        );
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

    #[test]
    fn compact_profile_is_wire_identical_to_the_existing_body() {
        let req = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("go")],
            tools: polaris_tools::all_specs(),
        };
        let existing = build_body("gpt-6-astra", &req, Some("medium"));
        let instructions =
            cache_prefix::CachePrefixProfile::Compact.instructions("gpt-6-astra", &req);
        let profiled =
            build_body_with_instructions("gpt-6-astra", &req, &instructions, Some("medium"));
        assert_eq!(profiled, existing, "compact changed the existing wire body");
        assert!(profiled.get("cache_prefix").is_none());
        assert!(profiled.get("cache_prefix_version").is_none());
    }

    #[test]
    fn stable_uses_a_distinct_key_that_holds_while_history_grows() {
        let turn = |n| CompletionRequest {
            system: "system".into(),
            messages: std::iter::once(Message::assistant_with_tool_calls(
                "read first",
                vec![ToolCall {
                    id: "call_preserved".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "Cargo.toml"}),
                }],
            ))
            .chain(std::iter::once(Message::tool_result(
                "call_preserved",
                "contents",
            )))
            .chain((0..n).map(|i| Message::user(format!("turn {i}"))))
            .collect(),
            tools: polaris_tools::all_specs(),
        };
        let first = turn(1);
        let appended = turn(2);
        let compact = build_body("gpt-6-astra", &first, Some("medium"));
        let stable_first_instructions =
            cache_prefix::CachePrefixProfile::Stable.instructions("gpt-6-astra", &first);
        let stable_appended_instructions =
            cache_prefix::CachePrefixProfile::Stable.instructions("gpt-6-astra", &appended);
        let stable_first = build_body_with_instructions(
            "gpt-6-astra",
            &first,
            &stable_first_instructions,
            Some("medium"),
        );
        let stable_appended = build_body_with_instructions(
            "gpt-6-astra",
            &appended,
            &stable_appended_instructions,
            Some("medium"),
        );
        assert_ne!(
            stable_first["prompt_cache_key"],
            compact["prompt_cache_key"]
        );
        assert_eq!(
            stable_first["prompt_cache_key"], stable_appended["prompt_cache_key"],
            "appending history moved the stable cache key"
        );
        assert_eq!(stable_first["input"], first_body_input(&first));
        assert_eq!(stable_appended["input"], first_body_input(&appended));
        assert_eq!(stable_first["input"][1]["call_id"], "call_preserved");
        assert_eq!(stable_first["tools"], compact["tools"]);
        for body in [&stable_first, &stable_appended] {
            assert!(body.get("cache_prefix").is_none());
            assert!(body.get("cache_prefix_version").is_none());
            assert!(body.get("cache_prefix_hash").is_none());
            assert!(body.get("target_applied").is_none());
        }
    }

    #[test]
    fn stable_skips_non_target_models_and_tool_less_requests() {
        let tools = polaris_tools::all_specs();
        let target = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("go")],
            tools: tools.clone(),
        };
        let tool_less = CompletionRequest {
            system: "system".into(),
            messages: vec![Message::user("go")],
            tools: vec![],
        };
        for (model, req) in [
            ("gpt-6-astra-preview", &target),
            ("gpt-6-astra", &tool_less),
        ] {
            let instructions = cache_prefix::CachePrefixProfile::Stable.instructions(model, req);
            let profiled = build_body_with_instructions(model, req, &instructions, Some("medium"));
            assert_eq!(profiled, build_body(model, req, Some("medium")));
            assert!(
                !cache_prefix::CachePrefixProfile::Stable
                    .diagnostics(model, req)
                    .target_applied
            );
        }
    }

    fn first_body_input(req: &CompletionRequest) -> Value {
        Value::Array(input_items(&req.messages))
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

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
        let r = p.complete(req()).await.expect("should succeed");
        assert_eq!(r.text, "ok");
    }

    #[tokio::test]
    async fn visible_request_limit_rejects_oversize_before_attempt_or_send() {
        let server = MockServer::start().await;
        let ledger = crate::attempts::AttemptLedger::default();
        let provider = CodexProvider::new(server.uri(), "m".into(), Tokens::new())
            .expect("client")
            .with_attempt_ledger(ledger.clone())
            .with_visible_request_byte_limit(1);

        let error = provider
            .complete(req())
            .await
            .expect_err("body exceeds one byte");
        assert!(matches!(error, ProviderError::Budget(_)));
        assert!(
            ledger.snapshot().is_empty(),
            "oversize body creates no attempt"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "oversize body sends nothing"
        );
    }

    #[tokio::test]
    async fn visible_request_limit_leaves_a_tiny_valid_body_unchanged() {
        let baseline_server = MockServer::start().await;
        let limited_server = MockServer::start().await;
        for server in [&baseline_server, &limited_server] {
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                    frame("response.output_item.done", message_item("ok")),
                    frame("response.completed", serde_json::json!({})),
                ])))
                .mount(server)
                .await;
        }

        CodexProvider::new(baseline_server.uri(), "m".into(), Tokens::new())
            .expect("client")
            .complete(req())
            .await
            .expect("baseline request succeeds");
        CodexProvider::new(limited_server.uri(), "m".into(), Tokens::new())
            .expect("client")
            .with_visible_request_byte_limit(32_000)
            .complete(req())
            .await
            .expect("limited request succeeds");

        let baseline = baseline_server.received_requests().await.expect("recorded");
        let limited = limited_server.received_requests().await.expect("recorded");
        assert_eq!(baseline.len(), 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(baseline[0].body, limited[0].body);
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

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
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
        let ledger = crate::attempts::AttemptLedger::default();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone())
            .expect("client")
            .with_attempt_ledger(ledger.clone());
        let r = p.complete(req()).await.expect("should succeed on retry");
        assert_eq!(r.text, "succeeded on retry");
        assert_eq!(
            t.refreshes.load(Ordering::SeqCst),
            1,
            "the number of refreshes isn't 1"
        );
        let attempts = ledger.snapshot();
        assert_eq!(attempts.len(), 2, "401 retry is two physical sends");
        assert_eq!(attempts[0].logical_id, attempts[1].logical_id);
        assert_eq!(attempts[0].status, crate::attempts::AttemptStatus::Failed);
        assert_eq!(
            attempts[1].status,
            crate::attempts::AttemptStatus::Succeeded
        );
        assert_eq!(
            attempts[1].retry_reason.as_deref(),
            Some("401 unauthorized")
        );
        assert_eq!(
            attempts[1].usage,
            crate::attempts::UsageObservation::Missing
        );
    }

    #[tokio::test]
    async fn ledger_marks_a_sent_future_cancelled_and_ignores_an_unpolled_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(200))
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
            )
            .mount(&server)
            .await;

        let ledger = crate::attempts::AttemptLedger::default();
        let provider = CodexProvider::new(server.uri(), "m".into(), Tokens::new())
            .expect("client")
            .with_attempt_ledger(ledger.clone());
        let never_polled = provider.complete(req());
        drop(never_polled);
        assert!(ledger.snapshot().is_empty(), "unpolled futures do not send");

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                provider.complete(req())
            )
            .await
            .is_err()
        );
        let attempts = ledger.snapshot();
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].status,
            crate::attempts::AttemptStatus::Cancelled
        );
        assert_eq!(
            attempts[0].usage,
            crate::attempts::UsageObservation::Missing
        );
    }

    #[tokio::test]
    async fn cache_pacing_preserves_wire_body_and_cancelled_wait_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame("response.output_item.done", message_item("ok")),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(&server)
            .await;
        let mut off =
            CodexProvider::new(server.uri(), "gpt-6-astra".into(), Tokens::new()).unwrap();
        off.pacing = PacingGate::new(PacingMode::Off);
        off.set_effort(Some("medium"));
        let mut on = CodexProvider::new(server.uri(), "gpt-6-astra".into(), Tokens::new()).unwrap();
        on.pacing =
            PacingGate::with_interval(PacingMode::On, std::time::Duration::from_millis(100));
        on.set_effort(Some("medium"));
        off.complete(req()).await.unwrap();
        on.complete(req()).await.unwrap();
        // Timeout owns and drops the actual request future, releasing its gate guard.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), on.complete(req()))
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        tokio::time::timeout(std::time::Duration::from_secs(1), on.complete(req()))
            .await
            .unwrap()
            .unwrap();
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 3);
        assert!(received.windows(2).all(|pair| pair[0].body == pair[1].body));
        assert!(
            received
                .iter()
                .all(|r| !r.headers.contains_key("x-codex-turn-state"))
        );
    }

    #[tokio::test]
    async fn cache_pacing_applies_again_to_the_single_auth_retry() {
        let server = MockServer::start().await;
        let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = starts.clone();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(move |request: &wiremock::Request| {
                observed.lock().unwrap().push(std::time::Instant::now());
                if request.headers.get("authorization").unwrap() == "Bearer first" {
                    ResponseTemplate::new(401)
                } else {
                    ResponseTemplate::new(200).set_body_string(sse_body(&[
                        frame("response.output_item.done", message_item("ok")),
                        frame("response.completed", serde_json::json!({})),
                    ]))
                }
            })
            .mount(&server)
            .await;
        let tokens = Tokens::new();
        let mut provider = CodexProvider::new(server.uri(), "m".into(), tokens.clone()).unwrap();
        provider.pacing =
            PacingGate::with_interval(PacingMode::On, std::time::Duration::from_millis(100));
        provider.complete(req()).await.unwrap();
        assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
        let starts = starts.lock().unwrap();
        assert_eq!(starts.len(), 2);
        // Allow local transport scheduling jitter; gate unit tests check the exact boundary.
        assert!(starts[1].duration_since(starts[0]) >= std::time::Duration::from_millis(80));
    }

    /// A readonly auth source rejects the refresh step after a 401. The
    /// provider must return that error directly: a second Responses request
    /// would reuse credentials that were already refused. The auth crate
    /// separately verifies that its readonly source makes no token POST or
    /// credential-store write before returning this error.
    #[tokio::test]
    async fn a_401_with_a_rejected_refresh_does_not_repeat_the_model_request() {
        struct ReadOnlyTokens {
            refreshes: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl crate::TokenSource for ReadOnlyTokens {
            async fn token(&self) -> Result<crate::Token, ProviderError> {
                Ok(crate::Token {
                    access_token: "first".into(),
                    account_id: "acct-1".into(),
                    effort: None,
                })
            }

            async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Auth(
                    "authentication refresh is disabled".into(),
                ))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .mount(&server)
            .await;

        let tokens = Arc::new(ReadOnlyTokens {
            refreshes: AtomicUsize::new(0),
        });
        let provider =
            CodexProvider::new(server.uri(), "m".into(), tokens.clone()).expect("client");
        let error = provider
            .complete(req())
            .await
            .expect_err("readonly refresh rejection must stop the request");

        assert!(
            matches!(error, ProviderError::Auth(_)),
            "not Auth: {error:?}"
        );
        assert_eq!(
            tokens.refreshes.load(Ordering::SeqCst),
            1,
            "the provider must ask the auth source exactly once"
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("request recording")
                .len(),
            1,
            "a rejected refresh must not repeat the model request"
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
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone()).expect("client");
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
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone()).expect("client");

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
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone()).expect("client");
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

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
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

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new()).expect("client");
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
        )
        .expect("client");
        let err = p.complete(req()).await.expect_err("should fail");
        assert!(matches!(err, ProviderError::Auth(_)), "not Auth: {err:?}");
    }

    fn affinity_provider(base: String, mode: TurnAffinityMode) -> CodexProvider {
        CodexProvider::with_idle_timeout_and_affinity(
            base,
            "m".into(),
            Tokens::new(),
            Duration::from_secs(1),
            mode,
        )
        .expect("client")
    }

    /// A state first observed after `response.completed` stays within its
    /// context. A fresh context, even on the same shared provider, has none.
    #[tokio::test]
    async fn turn_affinity_is_shared_only_within_one_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "opaque-state")
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
            )
            .mount(&server)
            .await;
        let provider = affinity_provider(server.uri(), TurnAffinityMode::On);
        let turn = TurnContext::new();
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("first response");
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("continuation");
        provider
            .complete_in_turn(req(), &TurnContext::new())
            .await
            .expect("new turn");

        let requests = server.received_requests().await.expect("recorded");
        assert!(requests[0].headers.get("x-codex-turn-state").is_none());
        assert_eq!(
            requests[1]
                .headers
                .get("x-codex-turn-state")
                .and_then(|value| value.to_str().ok()),
            Some("opaque-state")
        );
        assert!(requests[2].headers.get("x-codex-turn-state").is_none());
    }

    /// Absence is ordinary success, but must never manufacture a value for a
    /// later tool continuation.
    #[tokio::test]
    async fn missing_turn_state_is_never_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                "response.completed",
                serde_json::json!({}),
            )])))
            .mount(&server)
            .await;
        let provider = affinity_provider(server.uri(), TurnAffinityMode::On);
        let turn = TurnContext::new();
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("first response");
        provider
            .complete_in_turn(req(), &turn)
            .await
            .expect("continuation");
        for request in server.received_requests().await.expect("recorded") {
            assert!(request.headers.get("x-codex-turn-state").is_none());
        }
    }

    /// Header transport is the only on/off difference. The JSON body still
    /// contains the real six-tool catalog, encrypted reasoning and a tool
    /// output byte-for-byte unchanged.
    #[tokio::test]
    async fn turn_affinity_does_not_change_outbound_body_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "opaque-state")
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
            )
            .mount(&server)
            .await;
        let request = || CompletionRequest {
            system: "s".into(),
            messages: vec![
                Message::assistant("thinking").with_reasoning(vec![ReasoningItem {
                    id: "reasoning-id".into(),
                    encrypted_content: "encrypted-reasoning".into(),
                }]),
                Message::tool_result("call-1", "tool-output"),
            ],
            tools: polaris_tools::all_specs(),
        };
        assert_eq!(
            request().tools.len(),
            6,
            "the actual always-on catalog has six tools"
        );
        let off_turn = TurnContext::new();
        let off = affinity_provider(server.uri(), TurnAffinityMode::Off);
        off.complete_in_turn(request(), &off_turn)
            .await
            .expect("off initial response");
        off.complete_in_turn(request(), &off_turn)
            .await
            .expect("off continuation");
        let on_turn = TurnContext::new();
        let on = affinity_provider(server.uri(), TurnAffinityMode::On);
        on.complete_in_turn(request(), &on_turn)
            .await
            .expect("on initial response");
        on.complete_in_turn(request(), &on_turn)
            .await
            .expect("on continuation");
        let requests = server.received_requests().await.expect("recorded");
        assert!(requests[1].headers.get("x-codex-turn-state").is_none());
        assert!(requests[3].headers.get("x-codex-turn-state").is_some());
        assert_eq!(requests[1].body, requests[3].body);
    }

    fn sequence_responder(
        responses: Vec<ResponseTemplate>,
    ) -> impl Fn(&wiremock::Request) -> ResponseTemplate + Send + Sync {
        let responses = Arc::new(std::sync::Mutex::new(responses));
        move |_| responses.lock().expect("responses").remove(0)
    }

    #[tokio::test]
    async fn concurrent_contexts_keep_distinct_server_states_on_one_provider() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(sequence_responder(vec![
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "state-a")
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                ResponseTemplate::new(200)
                    .insert_header("x-codex-turn-state", "state-b")
                    .set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
                ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
            ]))
            .mount(&server)
            .await;
        let provider = Arc::new(affinity_provider(server.uri(), TurnAffinityMode::On));
        let a = Arc::new(TurnContext::new());
        let b = Arc::new(TurnContext::new());
        let (ra, rb) = tokio::join!(
            provider.complete_in_turn(req(), &a),
            provider.complete_in_turn(req(), &b)
        );
        ra.expect("a initial");
        rb.expect("b initial");
        let (ra, rb) = tokio::join!(
            provider.complete_in_turn(req(), &a),
            provider.complete_in_turn(req(), &b)
        );
        ra.expect("a continuation");
        rb.expect("b continuation");
        let requests = server.received_requests().await.expect("recorded");
        let sent: std::collections::BTreeSet<_> = requests[2..]
            .iter()
            .map(|r| {
                r.headers
                    .get("x-codex-turn-state")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            sent,
            ["state-a".to_string(), "state-b".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[tokio::test]
    async fn failed_cancelled_decode_and_empty_headers_never_replace_state() {
        let cases = vec![
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "new")
                .set_body_string(sse_body(&[frame("response.failed", serde_json::json!({}))])),
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "new")
                .set_body_string(sse_body(&[frame(
                    "response.cancelled",
                    serde_json::json!({}),
                )])),
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "new")
                .set_body_string("data: not-json\n\n"),
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "")
                .set_body_string(sse_body(&[frame(
                    "response.completed",
                    serde_json::json!({}),
                )])),
        ];
        for bad in cases {
            // First-state-wins alone would mask a bug that saves a failed
            // response's header. Exercise an initially empty context too.
            let fresh_server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(sequence_responder(vec![
                    bad.clone(),
                    ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                ]))
                .mount(&fresh_server)
                .await;
            let fresh_provider = affinity_provider(fresh_server.uri(), TurnAffinityMode::On);
            let fresh_turn = TurnContext::new();
            let _ = fresh_provider.complete_in_turn(req(), &fresh_turn).await;
            fresh_provider
                .complete_in_turn(req(), &fresh_turn)
                .await
                .expect("after initial bad response");
            let fresh_requests = fresh_server.received_requests().await.expect("recorded");
            assert!(
                fresh_requests[1]
                    .headers
                    .get("x-codex-turn-state")
                    .is_none()
            );
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(sequence_responder(vec![
                    ResponseTemplate::new(200)
                        .insert_header("x-codex-turn-state", "old")
                        .set_body_string(sse_body(&[frame(
                            "response.completed",
                            serde_json::json!({}),
                        )])),
                    bad,
                    ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                ]))
                .mount(&server)
                .await;
            let provider = affinity_provider(server.uri(), TurnAffinityMode::On);
            let turn = TurnContext::new();
            provider
                .complete_in_turn(req(), &turn)
                .await
                .expect("initial");
            let _ = provider.complete_in_turn(req(), &turn).await;
            provider
                .complete_in_turn(req(), &turn)
                .await
                .expect("after bad response");
            let requests = server.received_requests().await.expect("recorded");
            assert_eq!(
                requests[2]
                    .headers
                    .get("x-codex-turn-state")
                    .and_then(|v| v.to_str().ok()),
                Some("old")
            );
        }
    }

    #[tokio::test]
    async fn refresh_keeps_state_only_when_the_account_is_unchanged() {
        struct RefreshTokens {
            changed: bool,
        }
        #[async_trait::async_trait]
        impl crate::TokenSource for RefreshTokens {
            async fn token(&self) -> Result<crate::Token, ProviderError> {
                Ok(crate::Token {
                    access_token: "first".into(),
                    account_id: "a".into(),
                    effort: None,
                })
            }
            async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
                Ok(crate::Token {
                    access_token: "second".into(),
                    account_id: if self.changed { "b" } else { "a" }.into(),
                    effort: None,
                })
            }
        }
        for changed in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(sequence_responder(vec![
                    ResponseTemplate::new(200)
                        .insert_header("x-codex-turn-state", "old")
                        .set_body_string(sse_body(&[frame(
                            "response.completed",
                            serde_json::json!({}),
                        )])),
                    ResponseTemplate::new(401),
                    ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                        "response.completed",
                        serde_json::json!({}),
                    )])),
                ]))
                .mount(&server)
                .await;
            let provider = CodexProvider::with_idle_timeout_and_affinity(
                server.uri(),
                "m".into(),
                Arc::new(RefreshTokens { changed }),
                Duration::from_secs(1),
                TurnAffinityMode::On,
            )
            .expect("client");
            let turn = TurnContext::new();
            provider
                .complete_in_turn(req(), &turn)
                .await
                .expect("initial");
            provider
                .complete_in_turn(req(), &turn)
                .await
                .expect("refresh retry");
            let requests = server.received_requests().await.expect("recorded");
            assert_eq!(
                requests[1]
                    .headers
                    .get("x-codex-turn-state")
                    .and_then(|v| v.to_str().ok()),
                Some("old")
            );
            assert_eq!(
                requests[2].headers.get("x-codex-turn-state").is_some(),
                !changed
            );
        }
    }
}
