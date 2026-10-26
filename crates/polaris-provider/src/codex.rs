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

/// A per-process suffix for the cache routing key.
///
/// `POLARIS_CACHE_SCOPE=conversation`. Absent keeps the shared content hash,
/// which is what polaris has always sent.
///
/// The content hash alone is shared by every run of the same project with
/// the same tools — including agents running side by side — so they are all
/// routed to one shard and evict each other's prefix there. Upstream keys on
/// the session id for this reason. Measured on 2026-09-18: this changes no
/// cost by itself (the cache took 0 tokens either way on this transport),
/// and is kept because sending several conversations to one shard under one
/// key is wrong regardless of what it costs.
fn conversation_scoped_key() -> Option<&'static str> {
    use std::sync::OnceLock;
    static NONCE: OnceLock<Option<String>> = OnceLock::new();
    NONCE
        .get_or_init(|| {
            (std::env::var_os("POLARIS_CACHE_SCOPE").as_deref()
                == Some(std::ffi::OsStr::new("conversation")))
            .then(|| {
                let mut bytes = [0u8; 8];
                use std::io::Read;
                std::fs::File::open("/dev/urandom")
                    .and_then(|mut file| file.read_exact(&mut bytes))
                    .ok()
                    .map(|()| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
            })
            .flatten()
        })
        .as_deref()
}

#[path = "codex_metrics.rs"]
pub(crate) mod metrics;

#[path = "codex/fold.rs"]
mod fold;
pub use fold::Folder;

use crate::attempts::{AttemptLedger, LogicalRequest};
use crate::cache_pacing::{Gate as PacingGate, Mode as PacingMode};
use crate::turn_affinity::{TurnAffinityMode, TurnContext, TurnIdentity};
use crate::web_search::WebSearchConfig;
use crate::{CompletionRequest, CompletionResponse, Message, Provider, ProviderError, Role};

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

/// How much prose the model is asked for, as `text.verbosity`.
///
/// Upstream sends this whenever the model supports it
/// (`core/src/client.rs:873`); polaris has never sent a `text` field at
/// all. It is the one output control that is part of the API rather than a
/// sentence added to the prompt, and every prompt-side attempt to shorten
/// the answer was measured on 2026-09-21 and cost more than it saved.
/// Astra's output is 646 tokens a run, 24% of what the parity family
/// costs, so this is worth a measurement rather than an assumption.
fn verbosity() -> Option<String> {
    // Low by default. Measured on 2026-09-21 at $0.1288 against $0.1344
    // over n=12 with quality 36/36 — a 4% difference that did not
    // separate from noise on its own (t=1.42), which is why it sat behind
    // the variable until the cost target made the trade explicit. pi
    // ships `low` as its own default and answers the same three questions
    // correctly on a third of the output.
    let value = std::env::var("POLARIS_VERBOSITY").unwrap_or_else(|_| "low".to_owned());
    matches!(value.as_str(), "low" | "medium" | "high").then_some(value)
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
        //
        // It is not a choice either. Sending `true` on 2026-09-21 came back
        // `400 invalid_request_error: "Store must be set to false"`, so this
        // endpoint fixes the value and `store` cannot be why the backend
        // never writes polaris a prefix cache entry.
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
        "prompt_cache_key": match conversation_scoped_key() {
            // One conversation, one routing key. The content hash alone is
            // shared by every run of the same project with the same tools —
            // including agents running side by side — so they are sent to the
            // one shard and evict each other's prefix there. Upstream keys on
            // the session id for this reason.
            Some(nonce) => format!("{}-{nonce}", crate::cache_key(model, effort, &effective_req)),
            None => crate::cache_key(model, effort, &effective_req),
        },
    });
    if let Some(verbosity) = verbosity() {
        body["text"] = serde_json::json!({ "verbosity": verbosity });
    }
    let tools = tool_wire_shape(&req.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(e) = effort {
        body["reasoning"] = serde_json::json!({ "effort": e });
    }
    body
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
    body_deadline: Duration,
    cache_namespace: Option<std::ffi::OsString>,
    cache_mode: Option<std::ffi::OsString>,
    cache_prefix: cache_prefix::CachePrefixProfile,
    turn_affinity: TurnAffinityMode,
    pacing: PacingGate,
    attempt_ledger: Option<AttemptLedger>,
    web_search: WebSearchConfig,
    request_caps: Option<crate::attempts::RequestCaps>,
    /// The open socket, when this run uses one. Held here rather than per
    /// turn because the continuation it carries is what makes the transport
    /// worth having, and a socket reopened every turn carries none.
    socket: tokio::sync::Mutex<Option<crate::codex_ws::Session>>,
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
            body_deadline: crate::RESPONSE_DEADLINE,
            cache_namespace: std::env::var_os("POLARIS_CACHE_NAMESPACE"),
            cache_mode: std::env::var_os("POLARIS_CACHE_MODE"),
            cache_prefix: cache_prefix::CachePrefixProfile::from_env()?,
            turn_affinity,
            pacing: PacingGate::new(PacingMode::from_env()?),
            attempt_ledger: None,
            web_search: WebSearchConfig::default(),
            request_caps: None,
            socket: tokio::sync::Mutex::new(None),
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
    /// explicit `set_effort` call (from a fixed CLI run or the turn judge)
    /// always wins over the token's plan-derived value when one has been made.
    /// One round trip over the open socket, or `None` to use HTTP instead.
    ///
    /// `None` means "this transport could not answer" and is not a failure:
    /// the caller falls through. A failure the caller must see — the model
    /// refusing, a budget — comes back as `Some(Err(..))`.
    ///
    /// **The saving is in what is not sent.** When the socket holds a
    /// continuation whose shape matches and whose history this request
    /// extends, only the new items go out and the server supplies the rest
    /// from what it already holds. When it does not, the whole input goes
    /// out and the continuation is omitted — which is the same request the
    /// HTTP path would have made, minus the connection setup.
    async fn over_socket(
        &self,
        body: &serde_json::Value,
        token: &crate::Token,
        observer: Option<&dyn crate::ResponseObserver>,
        prefix: cache_prefix::CachePrefixDiagnostics,
    ) -> Option<Result<CompletionResponse, ProviderError>> {
        // A transport that silently falls back is a transport nobody can
        // tell is working. Every decision it makes is said out loud while it
        // is behind a flag.
        let note = |what: &str| eprintln!("[ws] {what}");
        let mut held = self.socket.lock().await;
        if held.is_none() {
            match crate::codex_ws::Session::open(&self.base, token).await {
                Ok(session) => {
                    note("connected");
                    *held = Some(session);
                }
                // The upgrade failed: no socket, no error to the caller.
                Err(error) => {
                    note(&format!("Cannot connect ({error:?}). Falling back to HTTP"));
                    return None;
                }
            }
        }
        let session = held.as_mut()?;
        let shape = crate::codex_ws::Shape::of(body);
        let input: Vec<serde_json::Value> = body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut frame = body.clone();
        // Always present on this transport even when empty or absent: the
        // server reads the frame by shape, and omitting a field it expects
        // is not the same as sending it empty.
        frame["tool_choice"] = serde_json::Value::String("auto".into());
        // Several calls in one reply, because the loop already takes them
        // that way — `dispatch` walks the list and `siblings_ok` decides
        // what a failed one does to the rest. Holding it at `false` made
        // the model spend a whole round trip per file it wanted to read,
        // and a round trip re-bills the entire conversation. Upstream
        // sends `true`; nothing here ever recorded a reason not to.
        frame["parallel_tool_calls"] = serde_json::Value::Bool(true);
        if frame.get("reasoning").is_none() {
            frame["reasoning"] = serde_json::Value::Null;
        }
        let mut continued = false;
        let mut sent = input.len();
        match session.continuation_for(&shape).and_then(|held| {
            crate::codex_ws::delta(&held.baseline, &input).map(|d| (held.response_id.clone(), d))
        }) {
            Some((previous, delta)) => {
                note(&format!(
                    "Continuation available. Sending {} items ({} total)",
                    delta.len(),
                    input.len()
                ));
                continued = true;
                sent = delta.len();
                frame["previous_response_id"] = serde_json::Value::String(previous);
                frame["input"] = serde_json::Value::Array(delta);
            }
            None => {
                note(&format!(
                    "No continuation ({}). Sending all {} items",
                    session.why_no_continuation(&shape, &input),
                    input.len()
                ));
                frame.as_object_mut()?.remove("previous_response_id");
            }
        }
        // Built from the frame actually put on the wire, not from the HTTP
        // body: what this arm has to answer for is the delta it sent and
        // the usage the server billed against the conversation it holds.
        let mut metric = match metrics::RequestMetric::from_env(&frame, prefix) {
            Ok(metric) => metric,
            // A metrics path that cannot be opened is a measurement
            // failure, not a transport failure. The HTTP path reports it by
            // failing the request; do the same rather than quietly
            // producing an unmeasured run that looks like a measured one.
            Err(error) => return Some(Err(error)),
        };
        if let Some(metric) = metric.as_mut() {
            metric.socket(continued, sent, input.len());
        }
        let answer = session.round_trip(frame, input, observer).await;
        if let Some(metric) = metric.as_mut() {
            if let Ok((_, usage)) = &answer {
                metric.usage = *usage;
            }
            metric.finish(&answer);
        }
        match answer {
            Ok((completed, usage)) => {
                note(&format!("Response {usage:?}"));
                Some(Ok(completed))
            }
            Err(error) => {
                note(&format!("Failed ({error:?}). Falling back to HTTP"));
                // A broken socket is not reusable: the server's idea of the
                // conversation and this one have parted ways.
                *held = None;
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        token: &crate::Token,
        req: &CompletionRequest,
        turn: Option<&TurnContext>,
        logical: Option<&LogicalRequest>,
        retry_reason: Option<&str>,
        observer: Option<&dyn crate::ResponseObserver>,
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
        // The socket, when this run uses one. Tried before the HTTP request
        // and never instead of it: a failure here — the upgrade, a frame, a
        // continuation the server no longer holds — falls through to the
        // path below rather than failing the run. The transport is an
        // optimisation; the run is not.
        let prefix_diagnostics = self.cache_prefix.diagnostics(&model, req);
        if crate::codex_ws::enabled()
            && let Some(answer) = self
                .over_socket(&body, token, observer, prefix_diagnostics)
                .await
        {
            return answer;
        }
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
        let mut delta_matches = None;
        let mut received_usage = None;
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
            let resp = tokio::time::timeout(self.idle, request.send())
                .await
                .map_err(|_| ProviderError::Http("response headers deadline exceeded".into()))?
                .map_err(|e| {
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
            if !status.is_success() {
                let hint = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("unknown")
                    .to_string();
                let bytes = crate::read_limited(
                    resp,
                    crate::MAX_ERROR_BYTES,
                    self.idle.min(crate::ERROR_DEADLINE),
                )
                .await?;
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    return Err(ProviderError::Auth(format!("status {status}")));
                }
                let body = String::from_utf8_lossy(&bytes);
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(ProviderError::Http(format!(
                        "rate limited. retry-after: {hint} seconds. {body}"
                    )));
                }
                return Err(ProviderError::Http(format!("status {status}: {body}")));
            }
            if resp
                .content_length()
                .is_some_and(|len| len > crate::MAX_RESPONSE_BYTES as u64)
            {
                return Err(ProviderError::Decode(
                    "HTTP response exceeds byte limit".into(),
                ));
            }

            let mut folder = Folder::new();
            let mut stream = resp.bytes_stream();
            let deadline = tokio::time::Instant::now() + self.body_deadline;
            loop {
                // Measured against idle time. The response's overall length
                // grows normally.
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(ProviderError::Http(
                        "HTTP body read deadline exceeded".into(),
                    ));
                }
                let next = tokio::time::timeout(self.idle.min(remaining), stream.next()).await;
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
                        let pushed = folder.push_observed(&bytes, observer);
                        received_usage = folder.usage;
                        if let Some(metric) = metric.as_mut() {
                            metric.usage = folder.metrics_usage;
                            metric.server_cancelled = folder.cancelled;
                        }
                        pushed?;
                    }
                }
            }
            delta_matches = folder.preview_matches();
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
        .await
        .map_err(|error: ProviderError| error.with_received_usage(received_usage));
        if let Some(guard) = attempt_guard.as_mut() {
            guard.finish(&result);
        }
        if let Some(metric) = metric.as_mut() {
            metric.finish(&result);
        }
        result.and_then(|response| crate::emit_final(response, observer, delta_matches))
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn planned_local_model_bytes(&self) -> Option<u64> {
        crate::model_memory::cloud_allocation(&self.base, "chatgpt.com")
    }
    fn network_boundary(&self) -> crate::NetworkBoundary {
        crate::NetworkBoundary::RemoteAllowed
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.validate_optional_contracts()?;
        let logical = self
            .attempt_ledger
            .as_ref()
            .map(AttemptLedger::begin_logical);
        let token = self.tokens.token().await?;
        match self
            .attempt(&token, &req, None, logical.as_ref(), None, None)
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
                    None,
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
        self.complete_in_turn_with_observer(req, turn, None).await
    }

    async fn complete_in_turn_with_observer(
        &self,
        req: CompletionRequest,
        turn: &TurnContext,
        observer: Option<&dyn crate::ResponseObserver>,
    ) -> Result<CompletionResponse, ProviderError> {
        self.validate_optional_contracts()?;
        let logical = self
            .attempt_ledger
            .as_ref()
            .map(AttemptLedger::begin_logical);
        let token = self.tokens.token().await?;
        match self
            .attempt(&token, &req, Some(turn), logical.as_ref(), None, observer)
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
                    observer,
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

    fn model(&self) -> Option<String> {
        Some(self.model.read().expect("model lock poisoned").clone())
    }

    fn set_effort(&self, effort: Option<&str>) {
        *self.effort_override.write().expect("effort lock poisoned") = effort.map(str::to_string);
    }
}

#[cfg(test)]
#[path = "codex/tests.rs"]
mod tests;
