//! Provider abstraction. Transport-dependent parts live in each
//! implementation; only the shape of requests and responses lives here.

pub mod codex;
pub mod openai;
pub mod sse;

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
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: Vec::new(),
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
        }
    }

    /// Attaches the reasoning items produced alongside this turn, so the
    /// Codex provider can replay them on the next request. A no-op for
    /// providers that don't carry the concept.
    pub fn with_reasoning(mut self, reasoning: Vec<ReasoningItem>) -> Self {
        self.reasoning = reasoning;
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
#[derive(Debug, Clone, Copy, Default)]
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
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("could not interpret response: {0}")]
    Decode(String),
    #[error("auth: {0}")]
    Auth(String),
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError>;

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
    use super::*;

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
        let evs = d.push(b"data: hello\n\n");
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
}
