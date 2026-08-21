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

/// A single message in the history. `tool_calls` is non-empty only when an
/// assistant turn called a tool, and `tool_call_id` is carried only by a
/// tool-result message. Both stay empty for an ordinary user/assistant
/// utterance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
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
        }
    }

    /// Records a tool result, tied to the id of the call it answers.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
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
}

#[derive(Debug, Clone, Default)]
pub struct CompletionResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
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
}
