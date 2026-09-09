//! Provider abstraction. Transport-dependent parts live in each
//! implementation; only the shape of requests and responses lives here.

pub mod attempts;
pub mod cache_pacing;
pub mod codex;
pub mod local;
pub mod local_inference;
pub mod openai;
pub mod role;
pub mod sse;
pub mod turn_affinity;
pub mod web_search;

use polaris_tools::ToolSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// A `reasoning` item as returned by the Responses API. `encrypted_content`
/// is opaque server-encrypted state — polaris never reads it, only replays
/// it verbatim on the next turn so the backend can resume the same chain
/// of thought across tool calls. Codex-provider-specific; `openai.rs`
/// (Chat Completions) has no equivalent concept and simply never populates
/// this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningItem {
    pub id: String,
    pub encrypted_content: String,
}

/// Application-level response limits, independent of the desktop event queue.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ERROR_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOOL_CALLS: usize = 128;
pub const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
pub const RESPONSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(900);
pub const ERROR_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub enum ResponseEvent<'a> {
    TextDelta {
        output_index: u64,
        content_index: u64,
        text: &'a str,
    },
    /// The completed response remains authoritative; false means replace the preview.
    FinalText {
        text: &'a str,
        delta_matches: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ObserverError {
    #[error("observer queue is full")]
    Full,
    #[error("observer is closed")]
    Closed,
    #[error("observer is cancelled")]
    Cancelled,
}

pub trait ResponseObserver: Send + Sync {
    /// Must not wait for queue space, block, or perform I/O. Failure stops the request.
    fn try_emit(&self, event: ResponseEvent<'_>) -> Result<(), ObserverError>;
}

pub(crate) fn check_limit(value: usize, limit: usize, what: &str) -> Result<(), ProviderError> {
    if value > limit {
        return Err(ProviderError::Decode(format!(
            "{what} exceeds byte/count limit {limit}"
        )));
    }
    Ok(())
}

pub(crate) fn checked_size(
    current: usize,
    added: usize,
    limit: usize,
    what: &str,
) -> Result<usize, ProviderError> {
    let size = current
        .checked_add(added)
        .ok_or_else(|| ProviderError::Decode(format!("{what} size overflow")))?;
    check_limit(size, limit, what)?;
    Ok(size)
}

/// Checks each received chunk before copying it into application-owned storage.
/// The total deadline also bounds a peer that sends an endless slow trickle.
pub(crate) async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
    deadline: std::time::Duration,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(ProviderError::Decode(format!(
            "HTTP response exceeds byte limit {limit}"
        )));
    }
    tokio::time::timeout(deadline, async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?
        {
            checked_size(bytes.len(), chunk.len(), limit, "HTTP response")?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ProviderError::Http("HTTP body read deadline exceeded".into()))?
}

pub(crate) fn emit_final(
    response: CompletionResponse,
    observer: Option<&dyn ResponseObserver>,
    delta_matches: Option<bool>,
) -> Result<CompletionResponse, ProviderError> {
    if let Some(observer) = observer
        && let Err(reason) = observer.try_emit(ResponseEvent::FinalText {
            text: &response.text,
            delta_matches,
        })
    {
        return Err(ProviderError::Observation {
            reason,
            completed: Some(Box::new(response)),
        });
    }
    Ok(response)
}

/// A single message in the history. `tool_calls` is non-empty only when an
/// assistant turn called a tool, and `tool_call_id` is carried only by a
/// tool-result message. Both stay empty for an ordinary user/assistant
/// utterance. `reasoning` is carried alongside an assistant turn for
/// providers that have the concept (Codex); it stays empty everywhere
/// else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<ReasoningItem>,
    /// Hosted provider items are preserved for display and session storage;
    /// they are never replayed as local function calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosted_web_search: Vec<web_search::HostedWebSearchItem>,
    /// URL citations remain attached to their provider text parts. Consumers
    /// must not reuse offsets after rewriting the text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub url_citations: Vec<web_search::UrlCitation>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: Vec::new(),
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: Vec::new(),
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
        }
    }

    /// Records an assistant turn together with its tool calls. Under
    /// OpenAI's round-trip contract, this message itself must remain in
    /// the history holding its `tool_calls` before the tool result is
    /// sent.
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            reasoning: Vec::new(),
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
        }
    }

    /// Records a tool result, tied to the id of the call it answers.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            reasoning: Vec::new(),
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
        }
    }

    /// Attaches the reasoning items produced alongside this turn, so the
    /// Codex provider can replay them on the next request. A no-op for
    /// providers that don't carry the concept.
    pub fn with_reasoning(mut self, reasoning: Vec<ReasoningItem>) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Attaches hosted Web output without changing the function-tool history
    /// or the disabled provider wire.
    pub fn with_hosted_web_search(
        mut self,
        hosted_web_search: Vec<web_search::HostedWebSearchItem>,
        url_citations: Vec<web_search::UrlCitation>,
    ) -> Self {
        self.hosted_web_search = hosted_web_search;
        self.url_citations = url_citations;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

/// FNV-1a, one chunk at a time. Written out rather than taken from the
/// standard library's `DefaultHasher` for the same reason
/// `polaris_core::project::project_id` writes it out: the standard
/// library doesn't specify its algorithm, and a value that shifted
/// across Rust versions would silently throw away a cache generation.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A stable routing hint for the provider's prompt cache, sent as
/// `prompt_cache_key`.
///
/// The provider caches by prefix, but on a load-balanced backend it has
/// to find the shard holding that prefix before it can reuse it. Without
/// a key, consecutive turns of one conversation can land on different
/// shards and miss a cache written moments earlier. Measured against the
/// Codex backend on 2026-08-25, polaris took 0 cached tokens across 8
/// requests while `codex` — which sends a key of its own — took 63.4%
/// on the same task.
///
/// The key covers exactly what fixes the cacheable prefix: the model,
/// the reasoning effort, the system prompt, and the tool definitions.
/// Requests sharing those share a prefix and belong on one shard;
/// requests that don't would only evict each other. Conversation
/// content is deliberately excluded — it grows every turn, and a key
/// that moved with it would point at a fresh shard each time, which is
/// the failure this exists to fix.
pub fn cache_key(model: &str, effort: Option<&str>, req: &CompletionRequest) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

    let mut hash = fnv1a(FNV_OFFSET_BASIS, model.as_bytes());
    hash = fnv1a(hash, b"\0");
    hash = fnv1a(hash, effort.unwrap_or_default().as_bytes());
    hash = fnv1a(hash, b"\0");
    hash = fnv1a(hash, req.system.as_bytes());
    for t in &req.tools {
        hash = fnv1a(hash, b"\0");
        hash = fnv1a(hash, t.name.as_bytes());
        hash = fnv1a(hash, b"\0");
        hash = fnv1a(hash, t.description.as_bytes());
        hash = fnv1a(hash, b"\0");
        hash = fnv1a(hash, t.parameters.to_string().as_bytes());
    }
    format!("polaris-{hash:016x}")
}

/// Token usage reported by a single provider response. `None` on
/// `CompletionResponse` means the provider's response didn't carry a
/// usable `usage` field — never a hard error, since this is a
/// after-the-fact report, not something the agent loop depends on to
/// function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    /// How many of `input_tokens` were served from the provider's prompt
    /// cache — `usage.prompt_tokens_details.cached_tokens` (Chat
    /// Completions) or `usage.input_tokens_details.cached_tokens`
    /// (Responses API / Codex). `0` both when truly zero and when the
    /// provider's response didn't carry the field at all — this is a
    /// display-only figure, not something the agent loop depends on.
    pub cached_tokens: u32,
}

/// Known consumption and coverage of a group of requests. Missing usage
/// and failed requests are not evidence of zero consumption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub usage: Usage,
    pub reported_responses: u64,
    pub missing_responses: u64,
    /// Failed requests without known usage, including dropped futures.
    /// Rejected responses with known usage and completed responses whose
    /// display failed are counted above instead.
    /// Consumption is unknown, not necessarily zero;
    /// an unpolled future never starts a request and is not counted.
    pub failed_requests: u64,
}

/// Shared observation of actual provider calls, including calls whose
/// results are discarded (empty summaries, schema retries, failed children).
#[derive(Clone, Default)]
pub struct UsageMeter(std::sync::Arc<std::sync::Mutex<UsageReport>>);

impl UsageMeter {
    pub fn snapshot(&self) -> UsageReport {
        *self.0.lock().expect("usage meter poisoned")
    }

    pub fn wrap<P>(&self, provider: P) -> MeteredProvider<P> {
        MeteredProvider {
            provider,
            meter: self.clone(),
        }
    }

    fn record(&self, result: &Result<CompletionResponse, ProviderError>) {
        let mut report = self.0.lock().expect("usage meter poisoned");
        let received = match result {
            Ok(response) => Some(response.usage),
            Err(ProviderError::Observation {
                completed: Some(response),
                ..
            }) => Some(response.usage),
            Err(ProviderError::ReceivedUsage { usage, .. }) => Some(Some(*usage)),
            Err(_) => None,
        };
        match received {
            Some(Some(usage)) => {
                report.reported_responses = report.reported_responses.saturating_add(1);
                let total = &mut report.usage;
                total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
                total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
                total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
                total.cached_tokens = total.cached_tokens.saturating_add(usage.cached_tokens);
            }
            Some(None) => report.missing_responses = report.missing_responses.saturating_add(1),
            None => report.failed_requests = report.failed_requests.saturating_add(1),
        }
    }
}

/// Accounts for cancellation when the async call is dropped at its await.
/// Normal completion disarms the guard after recording the response once.
struct RequestGuard<'a> {
    meter: &'a UsageMeter,
    completed: bool,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            // Drop may run during unwinding; do not panic again on poison.
            let mut report = self.meter.0.lock().unwrap_or_else(|e| e.into_inner());
            report.failed_requests = report.failed_requests.saturating_add(1);
        }
    }
}

/// Wrap each execution path once per meter; child totals must not then be
/// added again. A separate meter may observe a subset through this wrapper.
pub struct MeteredProvider<P> {
    provider: P,
    meter: UsageMeter,
}

#[async_trait::async_trait]
impl<P> Provider for MeteredProvider<P>
where
    P: std::ops::Deref + Send + Sync,
    P::Target: Provider,
{
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.complete_metered(req, None, None).await
    }

    async fn complete_in_turn(
        &self,
        req: CompletionRequest,
        turn: &turn_affinity::TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        self.complete_metered(req, Some(turn), None).await
    }

    async fn complete_in_turn_with_observer(
        &self,
        req: CompletionRequest,
        turn: &turn_affinity::TurnContext,
        observer: Option<&dyn ResponseObserver>,
    ) -> Result<CompletionResponse, ProviderError> {
        self.complete_metered(req, Some(turn), observer).await
    }

    fn resolve_role(
        &self,
        role: &str,
    ) -> Result<Option<std::sync::Arc<dyn Provider>>, ProviderError> {
        Ok(self.provider.resolve_role(role)?.map(|provider| {
            std::sync::Arc::new(self.meter.wrap(provider)) as std::sync::Arc<dyn Provider>
        }))
    }

    fn set_model(&self, model: &str) {
        self.provider.set_model(model);
    }

    fn set_effort(&self, effort: Option<&str>) {
        self.provider.set_effort(effort);
    }
}

impl<P> MeteredProvider<P>
where
    P: std::ops::Deref + Send + Sync,
    P::Target: Provider,
{
    async fn complete_metered(
        &self,
        req: CompletionRequest,
        turn: Option<&turn_affinity::TurnContext>,
        observer: Option<&dyn ResponseObserver>,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut guard = RequestGuard {
            meter: &self.meter,
            completed: false,
        };
        let result = match turn {
            Some(turn) => {
                self.provider
                    .complete_in_turn_with_observer(req, turn, observer)
                    .await
            }
            None => self.provider.complete(req).await,
        };
        self.meter.record(&result);
        guard.completed = true;
        result
    }
}

/// One provider round trip. `reasoning` is populated only by providers
/// that carry the concept (Codex); it's empty otherwise and meant to be
/// attached to the resulting `Message` via `with_reasoning` so it can be
/// replayed on the next turn.
#[derive(Debug, Clone, Default)]
pub struct CompletionResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning: Vec<ReasoningItem>,
    pub usage: Option<Usage>,
    pub hosted_web_search: Vec<web_search::HostedWebSearchItem>,
    pub url_citations: Vec<web_search::UrlCitation>,
}

impl CompletionResponse {
    /// Returns the model text followed by a safe, end-of-message source list.
    /// Citation offsets are deliberately ignored: the list remains valid even
    /// after a caller changes the text, and only parsed HTTP(S) URLs appear.
    pub fn display_text(&self) -> String {
        let mut sources: Vec<(String, String)> = Vec::new();
        for citation in &self.url_citations {
            let label = citation.title.as_deref().unwrap_or(&citation.url);
            push_display_source(&mut sources, label, &citation.url);
        }
        for item in &self.hosted_web_search {
            for source in &item.sources {
                push_display_source(&mut sources, &source.url, &source.url);
            }
        }
        if sources.is_empty() {
            return self.text.clone();
        }

        let mut display = self.text.clone();
        if !display.is_empty() {
            display.push_str("\n\n");
        }
        display.push_str("Sources:\n");
        for (index, (label, url)) in sources.into_iter().enumerate() {
            display.push_str(&format!(
                "{}- [{}](<{}>)\n",
                index + 1,
                escape_markdown_label(&label),
                url,
            ));
        }
        display.pop();
        display
    }
}

fn push_display_source(sources: &mut Vec<(String, String)>, label: &str, raw_url: &str) {
    let Ok(url) = reqwest::Url::parse(raw_url) else {
        return;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    let url = url.to_string();
    if !sources.iter().any(|(_, existing)| existing == &url) {
        sources.push((label.to_string(), url));
    }
}

fn escape_markdown_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(['\r', '\n'], " ")
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// A rejected response with known consumption, never executable output.
    #[error("{source}")]
    ReceivedUsage {
        #[source]
        source: Box<ProviderError>,
        usage: Usage,
    },
    #[error("response observation failed: {reason}")]
    Observation {
        reason: ObserverError,
        /// Present only after a complete provider response was received.
        /// Persist its usage even though delivery failed; do not execute its tools.
        completed: Option<Box<CompletionResponse>>,
    },
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("could not interpret response: {0}")]
    Decode(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("request budget: {0}")]
    Budget(String),
    #[error("unsupported provider capability: {0}")]
    Unsupported(String),
}

impl ProviderError {
    pub(crate) fn with_received_usage(self, usage: Option<Usage>) -> Self {
        match (self, usage) {
            (error @ Self::ReceivedUsage { .. }, _) => error,
            (source, Some(usage)) => Self::ReceivedUsage {
                source: Box::new(source),
                usage,
            },
            (source, None) => source,
        }
    }
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError>;

    /// Completes a request that belongs to one logical user turn. Providers
    /// without turn-scoped transport state retain their existing behavior.
    async fn complete_in_turn(
        &self,
        req: CompletionRequest,
        _turn: &turn_affinity::TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        self.complete(req).await
    }

    /// Optional progress hook. Legacy providers retain their request path.
    async fn complete_in_turn_with_observer(
        &self,
        req: CompletionRequest,
        turn: &turn_affinity::TurnContext,
        observer: Option<&dyn ResponseObserver>,
    ) -> Result<CompletionResponse, ProviderError> {
        let response = self.complete_in_turn(req, turn).await?;
        emit_final(response, observer, None)
    }

    /// Resolves a trusted agent type once, before any child request or retry.
    /// None preserves the legacy single-provider path. Configured routers must
    /// reject missing roles rather than silently falling back. Resolution itself
    /// performs no inference; wrappers must preserve their observation boundary.
    fn resolve_role(
        &self,
        _role: &str,
    ) -> Result<Option<std::sync::Arc<dyn Provider>>, ProviderError> {
        Ok(None)
    }

    /// Switches which model subsequent `complete` calls use. Takes `&self`
    /// (not `&mut self`) so it can be called through the same shared
    /// `&dyn Provider` reference the rest of the harness already holds —
    /// implementations that support it (`OpenAiProvider`, `CodexProvider`)
    /// use interior mutability. The default no-op is for implementations
    /// that don't need to support switching (test doubles, anything with
    /// a fixed model).
    fn set_model(&self, _model: &str) {}

    /// Switches the reasoning effort subsequent `complete` calls request,
    /// same `&self`-via-interior-mutability reasoning as `set_model`.
    /// `CodexProvider` otherwise derives this from the account's plan
    /// type (`Token::effort`) — an explicit `set_effort` call overrides
    /// that for the rest of the session. `None` restores the default
    /// (plan-derived for `CodexProvider`; unset for `OpenAiProvider`).
    fn set_effort(&self, _effort: Option<&str>) {}
}

// Forward every hook through pointer wrappers, including role resolution.
macro_rules! delegate_provider {
    ($pointer:ty) => {
        #[async_trait::async_trait]
        impl<T: Provider + ?Sized> Provider for $pointer {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                (**self).complete(req).await
            }
            async fn complete_in_turn(
                &self,
                req: CompletionRequest,
                turn: &turn_affinity::TurnContext,
            ) -> Result<CompletionResponse, ProviderError> {
                (**self).complete_in_turn(req, turn).await
            }
            async fn complete_in_turn_with_observer(
                &self,
                req: CompletionRequest,
                turn: &turn_affinity::TurnContext,
                observer: Option<&dyn ResponseObserver>,
            ) -> Result<CompletionResponse, ProviderError> {
                (**self)
                    .complete_in_turn_with_observer(req, turn, observer)
                    .await
            }
            fn resolve_role(
                &self,
                role: &str,
            ) -> Result<Option<std::sync::Arc<dyn Provider>>, ProviderError> {
                (**self).resolve_role(role)
            }
            fn set_model(&self, model: &str) {
                (**self).set_model(model);
            }
            fn set_effort(&self, effort: Option<&str>) {
                (**self).set_effort(effort);
            }
        }
    };
}
delegate_provider!(std::sync::Arc<T>);
delegate_provider!(&T);

/// The credentials used for a single request. The provider knows nothing
/// beyond this.
#[derive(Debug, Clone)]
pub struct Token {
    pub access_token: String,
    pub account_id: String,
    /// `reasoning.effort` sent to the Responses API. Its value comes from
    /// `polaris-auth`'s `chatgpt_plan_type`, or is `None` when it can't be
    /// decided. `polaris-provider` doesn't depend on `polaris-auth`, so it
    /// has no idea where the value came from — it just uses the string it
    /// was handed as-is.
    pub effort: Option<String>,
}

/// The source of tokens. `token()` returns "whatever is currently usable",
/// and `refreshed()` returns one refreshed regardless of expiry. The retry
/// after receiving a 401 uses the latter.
///
/// Placing this trait in `polaris-provider` and its implementation in
/// `polaris-cli` lets `polaris-auth` avoid depending on this crate. At the
/// same time, it means provider tests need no OAuth, no file, and no
/// browser.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<Token, ProviderError>;
    async fn refreshed(&self) -> Result<Token, ProviderError>;
}

#[cfg(test)]
mod tests {
    /// Local raw HTTP fixture: complete chunked data or a peer that keeps
    /// trickling bytes without terminating the body. No external addresses.
    pub(crate) async fn raw_reply(
        status: u16,
        body: Option<Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let worker = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            let header = format!(
                "HTTP/1.1 {status} fixture\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            );
            if socket.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            if let Some(body) = body {
                for chunk in body.chunks(8192) {
                    if socket
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                    if socket.write_all(b"\r\n").await.is_err() {
                        return;
                    }
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            } else {
                for _ in 0..200 {
                    if socket.write_all(b"1\r\nx\r\n").await.is_err() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
            }
        });
        (format!("http://{address}"), worker)
    }

    #[tokio::test]
    async fn p43_chunked_read_limits_and_trickle_deadline() {
        for limit in [MAX_ERROR_BYTES, MAX_RESPONSE_BYTES] {
            for size in [limit - 1, limit, limit + 1] {
                let (url, worker) = raw_reply(200, Some(vec![b'x'; size])).await;
                let response = reqwest::Client::new().get(url).send().await.unwrap();
                let result = read_limited(response, limit, std::time::Duration::from_secs(5)).await;
                assert_eq!(result.is_ok(), size <= limit, "size={size}");
                if let Ok(bytes) = result {
                    assert_eq!(bytes.len(), size);
                }
                worker.await.unwrap();
            }
        }
        let (url, worker) = raw_reply(500, None).await;
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            read_limited(
                response,
                MAX_ERROR_BYTES,
                std::time::Duration::from_millis(30),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(ref e) if e.to_string().contains("deadline")));
        worker.await.unwrap();
        assert!(checked_size(usize::MAX, 1, usize::MAX, "test").is_err());
    }

    use super::*;

    struct PendingProvider;

    #[async_trait::async_trait]
    impl Provider for PendingProvider {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            std::future::pending().await
        }
    }

    struct ErrorProvider;

    #[async_trait::async_trait]
    impl Provider for ErrorProvider {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Err(ProviderError::Http("failed".into()))
        }
    }

    fn empty_request() -> CompletionRequest {
        CompletionRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
        }
    }

    #[test]
    fn meter_records_dropped_polled_requests_as_unknown_once_per_scope() {
        let outer = UsageMeter::default();
        let inner = UsageMeter::default();
        let provider = outer.wrap(&PendingProvider);
        let wrapped = inner.wrap(&provider);

        drop(wrapped.complete(empty_request()));
        assert_eq!(outer.snapshot().failed_requests, 0);
        assert_eq!(inner.snapshot().failed_requests, 0);

        let mut request = wrapped.complete(empty_request());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(request.as_mut().poll(&mut context).is_pending());
        assert!(request.as_mut().poll(&mut context).is_pending());
        drop(request);

        for meter in [outer, inner] {
            let report = meter.snapshot();
            assert_eq!(report.failed_requests, 1);
            assert_eq!(report.reported_responses, 0);
            assert_eq!(report.missing_responses, 0);
            assert_eq!(report.usage.total_tokens, 0);
        }
    }

    #[tokio::test]
    async fn meter_disarms_drop_guard_for_success_missing_usage_and_error() {
        let meter = UsageMeter::default();
        let known = Canned {
            reply: CompletionResponse {
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                    total_tokens: 13,
                    cached_tokens: 5,
                }),
                ..Default::default()
            },
        };
        meter.wrap(&known).complete(empty_request()).await.unwrap();
        let report = meter.snapshot();
        assert_eq!(report.reported_responses, 1);
        assert_eq!(report.failed_requests, 0);
        assert_eq!(report.usage.total_tokens, 13);
        assert_eq!(report.usage.input_tokens, 10);
        assert_eq!(report.usage.output_tokens, 3);
        assert_eq!(report.usage.cached_tokens, 5);

        let missing = Canned {
            reply: CompletionResponse::default(),
        };
        meter
            .wrap(&missing)
            .complete(empty_request())
            .await
            .unwrap();
        assert_eq!(meter.snapshot().missing_responses, 1);
        assert_eq!(meter.snapshot().failed_requests, 0);

        assert!(
            meter
                .wrap(&ErrorProvider)
                .complete(empty_request())
                .await
                .is_err()
        );
        let report = meter.snapshot();
        assert_eq!(report.failed_requests, 1);
        assert_eq!(report.reported_responses, 1);
        assert_eq!(report.missing_responses, 1);
        assert_eq!(report.usage.total_tokens, 13);
    }

    #[tokio::test]
    async fn meter_distinguishes_zero_missing_failure_and_nested_scopes() {
        let outer = UsageMeter::default();
        let inner = UsageMeter::default();
        let provider = Canned {
            reply: CompletionResponse {
                usage: Some(Usage::default()),
                ..Default::default()
            },
        };
        let wrapped = outer.wrap(&provider);
        inner
            .wrap(&wrapped)
            .complete(CompletionRequest {
                system: String::new(),
                messages: vec![],
                tools: vec![],
            })
            .await
            .unwrap();
        assert_eq!(outer.snapshot().reported_responses, 1);
        assert_eq!(inner.snapshot().reported_responses, 1);
        outer.record(&Ok(CompletionResponse::default()));
        outer.record(&Err(ProviderError::Http("failed".into())));
        let report = outer.snapshot();
        assert_eq!(report.usage.total_tokens, 0);
        assert_eq!(report.reported_responses, 1);
        assert_eq!(report.missing_responses, 1);
        assert_eq!(report.failed_requests, 1);
    }

    #[test]
    fn meter_saturates_legacy_counters_instead_of_wrapping() {
        let meter = UsageMeter::default();
        let response = CompletionResponse {
            usage: Some(Usage {
                input_tokens: u32::MAX,
                output_tokens: 1,
                total_tokens: u32::MAX,
                cached_tokens: u32::MAX,
            }),
            ..Default::default()
        };
        meter.record(&Ok(response.clone()));
        meter.record(&Ok(response));
        let report = meter.snapshot();
        assert_eq!(report.usage.input_tokens, u32::MAX);
        assert_eq!(report.usage.output_tokens, 2);
        assert_eq!(report.usage.total_tokens, u32::MAX);
        assert_eq!(report.usage.cached_tokens, u32::MAX);
    }

    struct Canned {
        reply: CompletionResponse,
    }

    #[async_trait::async_trait]
    impl Provider for Canned {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn metered_provider_forwards_turn_context_and_records_usage_once() {
        struct TurnAware(std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl Provider for TurnAware {
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                panic!("complete_in_turn was not forwarded")
            }

            async fn complete_in_turn(
                &self,
                _req: CompletionRequest,
                _turn: &turn_affinity::TurnContext,
            ) -> Result<CompletionResponse, ProviderError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CompletionResponse {
                    usage: Some(Usage {
                        input_tokens: 7,
                        output_tokens: 2,
                        total_tokens: 9,
                        cached_tokens: 3,
                    }),
                    ..Default::default()
                })
            }
        }

        let provider = TurnAware(std::sync::atomic::AtomicUsize::new(0));
        let meter = UsageMeter::default();
        meter
            .wrap(&provider)
            .complete_in_turn(empty_request(), &turn_affinity::TurnContext::new())
            .await
            .expect("response");
        assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        let report = meter.snapshot();
        assert_eq!(report.reported_responses, 1);
        assert_eq!(report.usage.total_tokens, 9);
        assert_eq!(report.failed_requests, 0);
    }

    #[tokio::test]
    async fn provider_trait_is_object_safe_and_returns_tool_calls() {
        let p: Box<dyn Provider> = Box::new(Canned {
            reply: CompletionResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "src/main.rs" }),
                }],
                reasoning: Vec::new(),
                usage: None,
                ..CompletionResponse::default()
            },
        });
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
    }

    #[test]
    fn sse_decoder_is_reachable() {
        let mut d = sse::SseDecoder::new();
        let evs = d.push(b"data: hello\n\n").unwrap();
        assert_eq!(evs[0].data, "hello");
    }

    struct CannedTokens {
        first: String,
        second: String,
    }

    #[async_trait::async_trait]
    impl TokenSource for CannedTokens {
        async fn token(&self) -> Result<Token, ProviderError> {
            Ok(Token {
                access_token: self.first.clone(),
                account_id: "acct".into(),
                effort: None,
            })
        }
        async fn refreshed(&self) -> Result<Token, ProviderError> {
            Ok(Token {
                access_token: self.second.clone(),
                account_id: "acct".into(),
                effort: None,
            })
        }
    }

    #[tokio::test]
    async fn token_source_is_object_safe_and_distinguishes_refresh() {
        let s: Box<dyn TokenSource> = Box::new(CannedTokens {
            first: "a".into(),
            second: "b".into(),
        });
        assert_eq!(
            s.token().await.expect("should be obtainable").access_token,
            "a"
        );
        assert_eq!(
            s.refreshed()
                .await
                .expect("should be obtainable")
                .access_token,
            "b"
        );
    }

    /// An auth failure is a different kind from a model failure. They
    /// must be distinguishable by type, not by wording. Distinguishing by
    /// wording breaks the moment the message is edited.
    #[test]
    fn an_auth_error_is_its_own_variant() {
        let e = ProviderError::Auth("not logged in".into());
        assert!(matches!(e, ProviderError::Auth(_)));
        assert!(
            !matches!(ProviderError::Http("x".into()), ProviderError::Auth(_)),
            "Http matched Auth"
        );
    }

    #[test]
    fn a_message_with_reasoning_round_trips_through_json() {
        let m = Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
        )
        .with_reasoning(vec![ReasoningItem {
            id: "r1".into(),
            encrypted_content: "opaque".into(),
        }]);

        let json = serde_json::to_value(&m).expect("should serialize");
        assert_eq!(json["reasoning"][0]["id"], "r1");
        assert_eq!(json["reasoning"][0]["encrypted_content"], "opaque");

        let back: Message = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(back.reasoning.len(), 1);
        assert_eq!(back.reasoning[0], m.reasoning[0]);
    }

    #[test]
    fn a_message_without_reasoning_omits_the_field_from_json() {
        let m = Message::user("hello");
        let json = serde_json::to_value(&m).expect("should serialize");
        assert!(
            json.get("reasoning").is_none(),
            "empty reasoning should be omitted, not serialized as []"
        );
    }

    #[test]
    fn display_text_uses_only_safe_urls_and_never_uses_citation_offsets() {
        let response = CompletionResponse {
            text: "Answer".into(),
            url_citations: vec![web_search::UrlCitation {
                url: "https://example.test/a".into(),
                title: Some("[A]\\label".into()),
                start_index: Some(99),
                end_index: Some(1),
            }],
            hosted_web_search: vec![web_search::HostedWebSearchItem {
                output_index: 1,
                id: "ws_1".into(),
                status: web_search::HostedItemStatus::Completed,
                action: web_search::WebSearchAction::Search { queries: vec![] },
                sources: vec![
                    web_search::WebSource {
                        url: "https://example.test/a".into(),
                    },
                    web_search::WebSource {
                        url: "javascript:alert(1)".into(),
                    },
                ],
            }],
            ..CompletionResponse::default()
        };

        assert_eq!(
            response.display_text(),
            "Answer\n\nSources:\n1- [\\[A\\]\\\\label](<https://example.test/a>)"
        );
    }
}
