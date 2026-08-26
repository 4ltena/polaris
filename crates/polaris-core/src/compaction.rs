//! Automatic history summarization. Fires when the conversation's measured
//! token count crosses a fixed ceiling, replacing everything before the
//! most recent few user turns with one LLM-generated summary message.

use crate::budget::count_tokens;
#[cfg(test)]
use polaris_provider::ToolCall;
use polaris_provider::{CompletionRequest, Message, Provider, ProviderError, Role};

/// Conservative and model-agnostic — polaris has no per-model context
/// window table (no provider exposes one), so this is picked well below
/// the smallest context window in common use (128k+) rather than tuned to
/// any specific model.
pub const COMPACTION_THRESHOLD: usize = 100_000;

/// How many of the most recent user turns survive compaction verbatim.
pub const KEEP_RECENT_USER_TURNS: usize = 2;

const SUMMARIZE_INSTRUCTION: &str = "Summarize everything above as a \
    handoff for continuing this conversation. Cover: what the user \
    originally asked for, what has been done so far, decisions made and \
    why, any constraints or facts established, and what remains to be \
    done. Be factual and concise — this replaces the full transcript, so \
    include only what a continuation would actually need.";

const SUMMARY_PREFIX: &str = "This is a summary of the earlier part of \
    this conversation, produced automatically because it grew too large \
    to keep in full:\n\n";

const COMPACTION_SYSTEM_PROMPT: &str = "You are summarizing a coding \
    agent's conversation history so it can continue with less context. \
    Write only the summary — no preamble, no meta-commentary about the \
    summarization itself.";

/// Sums `count_tokens` over every `Message`'s content, its tool_calls (in
/// the same JSON shape actually sent on the wire), and its reasoning
/// items' encrypted_content — the same bytes that ride the wire on
/// replay, even though the content itself is opaque.
pub fn session_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            let mut total = count_tokens(&m.content);
            for c in &m.tool_calls {
                total += count_tokens(&c.arguments.to_string());
                total += count_tokens(&c.name);
            }
            for r in &m.reasoning {
                total += count_tokens(&r.encrypted_content);
            }
            total
        })
        .sum()
}

pub fn should_compact(total_tokens: usize) -> bool {
    total_tokens >= COMPACTION_THRESHOLD
}

/// Returns the index to cut at: the index of the `KEEP_RECENT_USER_TURNS`
/// most recent `Role::User` messages' *earliest* one — i.e. where the kept
/// tail begins. Returns 0 (nothing to compact) when there are
/// `KEEP_RECENT_USER_TURNS` or fewer user turns total.
fn cut_index(messages: &[Message]) -> usize {
    let user_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::User))
        .map(|(i, _)| i)
        .collect();
    if user_positions.len() <= KEEP_RECENT_USER_TURNS {
        return 0;
    }
    user_positions[user_positions.len() - KEEP_RECENT_USER_TURNS]
}

pub struct CompactionReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

/// Returns `Ok(None)` when there's nothing old enough to compact away
/// (`cut_index` returned 0) — not an error, just a no-op.
pub async fn compact(
    provider: &dyn Provider,
    messages: &mut Vec<Message>,
) -> Result<Option<CompactionReport>, ProviderError> {
    let cut = cut_index(messages);
    if cut == 0 {
        return Ok(None);
    }

    let messages_before = messages.len();
    let tokens_before = session_tokens(messages);

    let mut to_summarize = messages[..cut].to_vec();
    to_summarize.push(Message::user(SUMMARIZE_INSTRUCTION));
    let res = provider
        .complete(CompletionRequest {
            system: COMPACTION_SYSTEM_PROMPT.to_string(),
            messages: to_summarize,
            tools: vec![],
        })
        .await?;

    let mut new_messages = vec![Message::user(format!("{SUMMARY_PREFIX}{}", res.text))];
    new_messages.extend_from_slice(&messages[cut..]);
    *messages = new_messages;

    Ok(Some(CompactionReport {
        messages_before,
        messages_after: messages.len(),
        tokens_before,
        tokens_after: session_tokens(messages),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> Message {
        Message::user(content)
    }

    fn assistant_with_call(content: &str, call_id: &str) -> Message {
        Message::assistant_with_tool_calls(
            content,
            vec![ToolCall {
                id: call_id.to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
            }],
        )
    }

    fn tool_result(call_id: &str, content: &str) -> Message {
        Message::tool_result(call_id, content)
    }

    #[test]
    fn session_tokens_sums_content_tool_calls_and_reasoning() {
        let plain = session_tokens(&[user("hello")]);
        assert!(plain > 0);

        let with_call = session_tokens(&[assistant_with_call("", "c1")]);
        assert!(with_call > 0);

        let with_reasoning = session_tokens(&[Message::assistant("done").with_reasoning(vec![
            polaris_provider::ReasoningItem {
                id: "r1".into(),
                encrypted_content: "x".repeat(1000),
            },
        ])]);
        let without_reasoning = session_tokens(&[Message::assistant("done")]);
        assert!(
            with_reasoning > without_reasoning,
            "a large encrypted_content blob must count toward the total"
        );
    }

    #[test]
    fn should_compact_trips_at_the_threshold_not_before() {
        assert!(!should_compact(COMPACTION_THRESHOLD - 1));
        assert!(should_compact(COMPACTION_THRESHOLD));
        assert!(should_compact(COMPACTION_THRESHOLD + 1));
    }

    #[test]
    fn cut_index_keeps_exactly_the_recent_user_turns() {
        let messages = vec![
            user("turn 1"),
            Message::assistant("reply 1"),
            user("turn 2"),
            Message::assistant("reply 2"),
            user("turn 3"),
            Message::assistant("reply 3"),
        ];
        // KEEP_RECENT_USER_TURNS = 2 → keep turn 2 and turn 3, cut at turn 2's index (2).
        assert_eq!(cut_index(&messages), 2);
    }

    #[test]
    fn cut_index_is_zero_when_there_are_not_more_user_turns_than_the_keep_count() {
        let exactly_two = vec![
            user("turn 1"),
            Message::assistant("reply 1"),
            user("turn 2"),
            Message::assistant("reply 2"),
        ];
        assert_eq!(
            cut_index(&exactly_two),
            0,
            "exactly KEEP_RECENT_USER_TURNS turns — nothing to compact"
        );

        let one = vec![user("only turn")];
        assert_eq!(cut_index(&one), 0);

        let none: Vec<Message> = vec![];
        assert_eq!(cut_index(&none), 0);
    }

    #[test]
    fn cut_index_never_splits_a_tool_call_from_its_result() {
        let messages = vec![
            user("turn 1"),
            assistant_with_call("", "c1"),
            tool_result("c1", "result 1"),
            Message::assistant("reply 1"),
            user("turn 2"),
            assistant_with_call("", "c2"),
            tool_result("c2", "result 2"),
            Message::assistant("reply 2"),
            user("turn 3"),
            Message::assistant("reply 3"),
        ];
        let cut = cut_index(&messages);
        assert!(
            matches!(messages[cut].role, Role::User),
            "cut must land exactly on a Role::User message, index {cut} is {:?}",
            messages[cut].role
        );
    }

    struct Summarizer;

    #[async_trait::async_trait]
    impl Provider for Summarizer {
        async fn complete(
            &self,
            req: CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, ProviderError> {
            assert_eq!(req.system, COMPACTION_SYSTEM_PROMPT);
            assert!(req.tools.is_empty(), "summarization must not offer tools");
            Ok(polaris_provider::CompletionResponse {
                text: "the user asked X, we did Y".to_string(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn compact_replaces_old_turns_with_one_summary_and_keeps_the_recent_tail() {
        let mut messages = vec![
            user("turn 1"),
            Message::assistant("reply 1"),
            user("turn 2"),
            Message::assistant("reply 2"),
            user("turn 3"),
            Message::assistant("reply 3"),
        ];
        let before = messages.len();

        let report = compact(&Summarizer, &mut messages)
            .await
            .expect("should succeed")
            .expect("should have compacted something");

        assert_eq!(report.messages_before, before);
        // 1 summary message + the kept tail (turn 2, reply 2, turn 3, reply 3 = 4).
        assert_eq!(messages.len(), 5);
        assert_eq!(report.messages_after, 5);
        assert!(matches!(messages[0].role, Role::User));
        assert!(messages[0].content.starts_with(SUMMARY_PREFIX));
        assert!(messages[0].content.contains("the user asked X, we did Y"));
        assert_eq!(messages[1].content, "turn 2");
        assert_eq!(messages[4].content, "reply 3");
    }

    #[tokio::test]
    async fn compact_is_a_no_op_when_nothing_is_old_enough() {
        let mut messages = vec![user("only turn")];
        let original = messages.clone();

        let report = compact(&Summarizer, &mut messages)
            .await
            .expect("should succeed");

        assert!(report.is_none());
        assert_eq!(messages.len(), original.len());
        assert_eq!(messages[0].content, original[0].content);
    }
}
