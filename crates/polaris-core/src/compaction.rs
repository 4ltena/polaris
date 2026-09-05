//! Automatic history summarization. Fires when the conversation's measured
//! token count crosses a fixed ceiling, replacing everything before the
//! most recent few user turns with one LLM-generated summary message.

use crate::budget::count_tokens;
#[cfg(test)]
use polaris_provider::ToolCall;
use polaris_provider::{CompletionRequest, Message, Provider, ProviderError, Role};

/// Model-agnostic — polaris has no per-model context window table (no
/// provider exposes one). Originally 100k, picked well below the smallest
/// context window in common use (128k+) purely to bound growth. Raised to
/// 200k on the reasoning that compacting often is self-defeating: each
/// compaction replaces old turns with a freshly generated
/// (non-deterministic) summary, which discards the stable prefix the
/// prompt cache had been reusing.
///
/// Two things about that reasoning are worth stating outright rather than
/// leaving implied.
///
/// It is not a measured result. The comparable cache-efficiency claim made
/// for `prompt_cache_key` and `include` in v0.7.0 did not survive a direct
/// before/after build comparison — see that CHANGELOG entry — and this one
/// has not been put through the same test.
///
/// Past the smallest common window, this constant stops protecting
/// anything. A model whose window is under the threshold reaches its own
/// limit before compaction can fire, so for those models the feature is
/// inert rather than merely late. That is acceptable while polaris targets
/// gpt-5.x windows, and it is the first thing to revisit if a provider
/// with a smaller window is added.
pub const COMPACTION_THRESHOLD: usize = 200_000;

/// How many of the most recent user turns survive compaction verbatim.
pub const KEEP_RECENT_USER_TURNS: usize = 2;

/// The floor below which a prefix isn't worth summarizing. Without this,
/// a pathologically large recent turn (see `compact`'s doc) leaves
/// `cut_index` returning a small nonzero cut every subsequent call — after
/// the first compaction, the summary message it inserts is itself
/// `Role::User`, so the next cut lands right after it and `compact` would
/// otherwise re-summarize just that previous summary by itself, every
/// turn: a wasted provider round-trip, a bogus
/// `messages_before == messages_after` notice, and an unnecessary
/// prompt-cache invalidation, repeating until the real oversized tail
/// shrinks below threshold some other way.
pub const MIN_TOKENS_TO_SUMMARIZE: usize = 1_000;

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

#[derive(Debug)]
pub struct CompactionReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

pub type ArchiveHook = dyn Fn(&[Message]) -> std::io::Result<()> + Send + Sync;

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("could not archive history: {0}")]
    Archive(#[from] std::io::Error),
}

/// Usage survives an empty summary or a failure to archive the original.
#[derive(Debug)]
pub struct CompactionOutcome {
    pub result: Result<Option<CompactionReport>, CompactionError>,
    pub usage_report: polaris_provider::UsageReport,
}

pub async fn compact_with_usage(
    provider: &dyn Provider,
    messages: &mut Vec<Message>,
    before_compact: Option<&ArchiveHook>,
) -> CompactionOutcome {
    let meter = polaris_provider::UsageMeter::default();
    let result = compact_with_archive(&meter.wrap(provider), messages, before_compact).await;
    CompactionOutcome {
        result,
        usage_report: meter.snapshot(),
    }
}

/// Returns `Ok(None)` — not an error, just a no-op — when there's nothing
/// worth compacting: `cut_index` returned 0 (not enough history yet), the
/// prefix it found is below `MIN_TOKENS_TO_SUMMARIZE`, or the provider's
/// summary came back empty.
pub async fn compact(
    provider: &dyn Provider,
    messages: &mut Vec<Message>,
) -> Result<Option<CompactionReport>, ProviderError> {
    match compact_with_archive(provider, messages, None).await {
        Ok(report) => Ok(report),
        Err(CompactionError::Provider(error)) => Err(error),
        Err(CompactionError::Archive(_)) => unreachable!("no archive hook supplied"),
    }
}

pub async fn compact_with_archive(
    provider: &dyn Provider,
    messages: &mut Vec<Message>,
    before_compact: Option<&ArchiveHook>,
) -> Result<Option<CompactionReport>, CompactionError> {
    let cut = cut_index(messages);
    if cut == 0 || session_tokens(&messages[..cut]) < MIN_TOKENS_TO_SUMMARIZE {
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

    // A provider response with empty (or whitespace-only) text would
    // otherwise still overwrite the entire prefix with a near-empty
    // summary, destroying that history irrecoverably — and the caller
    // (`persist::rewrite`) would then write that loss to disk. Treat it
    // like "nothing to compact this turn" instead.
    if res.text.trim().is_empty() {
        return Ok(None);
    }

    if let Some(archive) = before_compact {
        archive(messages)?;
    }

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

    struct MeasuredSummary(&'static str);

    #[async_trait::async_trait]
    impl Provider for MeasuredSummary {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, ProviderError> {
            Ok(polaris_provider::CompletionResponse {
                text: self.0.into(),
                usage: Some(polaris_provider::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    total_tokens: 110,
                    cached_tokens: 50,
                }),
                ..Default::default()
            })
        }
    }

    fn compactable_history() -> Vec<Message> {
        vec![
            Message::user("old"),
            Message::assistant("x".repeat(MIN_TOKENS_TO_SUMMARIZE * 10)),
            Message::user("recent"),
            Message::user("latest"),
        ]
    }

    #[tokio::test]
    async fn measured_empty_summary_keeps_usage_without_archiving_or_replacing() {
        let mut messages = compactable_history();
        let original = serde_json::to_value(&messages).unwrap();
        let archive =
            |_: &[Message]| -> std::io::Result<()> { panic!("empty summary must not archive") };
        let outcome =
            compact_with_usage(&MeasuredSummary(" \n"), &mut messages, Some(&archive)).await;
        assert!(outcome.result.unwrap().is_none());
        assert_eq!(outcome.usage_report.usage.total_tokens, 110);
        assert_eq!(outcome.usage_report.reported_responses, 1);
        assert_eq!(serde_json::to_value(&messages).unwrap(), original);
    }

    #[tokio::test]
    async fn measured_archive_failure_preserves_full_history_and_usage() {
        let mut messages = compactable_history();
        let original = serde_json::to_value(&messages).unwrap();
        let expected = original.clone();
        let archive = move |history: &[Message]| -> std::io::Result<()> {
            assert_eq!(serde_json::to_value(history).unwrap(), expected);
            Err(std::io::Error::other("archive unavailable"))
        };
        let outcome =
            compact_with_usage(&MeasuredSummary("summary"), &mut messages, Some(&archive)).await;
        assert!(matches!(outcome.result, Err(CompactionError::Archive(_))));
        assert_eq!(outcome.usage_report.usage.total_tokens, 110);
        assert_eq!(serde_json::to_value(&messages).unwrap(), original);
    }

    #[tokio::test]
    async fn archive_receives_original_history_before_successful_replacement() {
        let mut messages = compactable_history();
        let original = serde_json::to_value(&messages).unwrap();
        let archived = std::sync::Arc::new(std::sync::Mutex::new(None));
        let saved = archived.clone();
        let archive = move |history: &[Message]| -> std::io::Result<()> {
            *saved.lock().unwrap() = Some(serde_json::to_value(history).unwrap());
            Ok(())
        };
        let outcome =
            compact_with_usage(&MeasuredSummary("summary"), &mut messages, Some(&archive)).await;
        assert!(outcome.result.unwrap().is_some());
        assert_eq!(archived.lock().unwrap().as_ref(), Some(&original));
        assert_eq!(messages.len(), 3);
        assert_eq!(outcome.usage_report.usage.cached_tokens, 50);
    }

    #[tokio::test]
    async fn measured_no_op_is_not_a_missing_response() {
        let outcome = compact_with_usage(
            &MeasuredSummary("summary"),
            &mut vec![Message::user("new")],
            None,
        )
        .await;
        assert!(outcome.result.unwrap().is_none());
        assert_eq!(outcome.usage_report.reported_responses, 0);
        assert_eq!(outcome.usage_report.missing_responses, 0);
    }

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
        // "reply 1" alone is well under `MIN_TOKENS_TO_SUMMARIZE` (Fix
        // 5a's floor guard) — inflated here so the summarized prefix
        // actually clears the floor and this test still exercises a real
        // compaction rather than tripping the new "not worth it" no-op.
        // Single-char repeats compress ~8:1 under o200k_base (see
        // `polaris-core::agent`'s own compaction test for the same note),
        // so 10x the floor in characters comfortably clears it in tokens.
        let big_reply = "x".repeat(MIN_TOKENS_TO_SUMMARIZE * 10);
        let mut messages = vec![
            user("turn 1"),
            Message::assistant(big_reply),
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

    /// Fix 5a's floor guard: `cut_index` alone can return a nonzero cut
    /// for a prefix that's too small to be worth a whole provider
    /// round-trip to summarize — the pathological case being the summary
    /// message compaction itself inserts (`Role::User`), which after a
    /// first compaction can make the *next* cut land right after it,
    /// re-summarizing just that one small message every turn. `compact`
    /// must skip a prefix under `MIN_TOKENS_TO_SUMMARIZE`, exactly like
    /// the `cut == 0` no-op.
    #[tokio::test]
    async fn compact_is_a_no_op_when_the_prefix_to_summarize_is_below_the_floor() {
        let mut messages = vec![
            user("tiny turn 1"),
            Message::assistant("tiny reply 1"),
            user("turn 2"),
            Message::assistant("reply 2"),
            user("turn 3"),
            Message::assistant("reply 3"),
        ];
        let original = messages.clone();
        // `cut_index` alone returns a nonzero cut here (same shape as
        // `cut_index_keeps_exactly_the_recent_user_turns`) — the floor
        // guard is what must turn it into a no-op, not the cut itself.
        assert_ne!(cut_index(&messages), 0);

        let report = compact(&Summarizer, &mut messages)
            .await
            .expect("should succeed");

        assert!(
            report.is_none(),
            "a prefix this small should be skipped as not worth summarizing"
        );
        assert_eq!(messages.len(), original.len());
        for (m, o) in messages.iter().zip(original.iter()) {
            assert_eq!(m.content, o.content);
        }
    }

    /// Fix 3: an empty (or whitespace-only) summary response must not
    /// overwrite the prefix it was meant to summarize — that would
    /// destroy that history irrecoverably (and `persist::rewrite` would
    /// then write the loss to disk).
    struct EmptySummarizer;

    #[async_trait::async_trait]
    impl Provider for EmptySummarizer {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, ProviderError> {
            Ok(polaris_provider::CompletionResponse {
                text: "   \n  ".to_string(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn compact_leaves_messages_unchanged_when_the_summary_response_is_empty() {
        let big_reply = "x".repeat(MIN_TOKENS_TO_SUMMARIZE * 10);
        let mut messages = vec![
            user("turn 1"),
            Message::assistant(big_reply),
            user("turn 2"),
            Message::assistant("reply 2"),
            user("turn 3"),
            Message::assistant("reply 3"),
        ];
        let original = messages.clone();

        let report = compact(&EmptySummarizer, &mut messages)
            .await
            .expect("should succeed");

        assert!(
            report.is_none(),
            "a whitespace-only summary must not be treated as a real compaction"
        );
        assert_eq!(messages.len(), original.len());
        for (m, o) in messages.iter().zip(original.iter()) {
            assert_eq!(m.content, o.content);
        }
    }
}
