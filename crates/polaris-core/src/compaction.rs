//! Automatic history summarization. Fires when the conversation's measured
//! token count crosses a fixed ceiling. Keeps recent user turns, or the
//! original instruction and recent complete tool groups in a single turn.

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
/// most recent real user messages' *earliest* one — i.e. where the kept
/// tail begins. Returns 0 (nothing to compact) when there are
/// `KEEP_RECENT_USER_TURNS` or fewer user turns total.
fn cut_index(messages: &[Message]) -> usize {
    let user_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_user_instruction(m))
        .map(|(i, _)| i)
        .collect();
    if user_positions.len() <= KEEP_RECENT_USER_TURNS {
        return 0;
    }
    user_positions[user_positions.len() - KEEP_RECENT_USER_TURNS]
}

fn is_user_instruction(message: &Message) -> bool {
    matches!(message.role, Role::User) && !message.content.starts_with(SUMMARY_PREFIX)
}

/// Only cut at boundaries with no outstanding calls. Invalid or ambiguous
/// pairing disables compaction rather than risking an orphaned result.
fn compaction_range(messages: &[Message]) -> Option<std::ops::Range<usize>> {
    let mut pending = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();
    let mut boundaries = vec![0];
    let mut groups = Vec::new();
    let mut group_start = None;
    for (index, message) in messages.iter().enumerate() {
        if !message.tool_calls.is_empty() {
            if !matches!(message.role, Role::Assistant) {
                return None;
            }
            group_start.get_or_insert(index);
            for call in &message.tool_calls {
                if !seen.insert(call.id.as_str()) {
                    return None;
                }
                pending.insert(call.id.as_str());
            }
        }
        if matches!(message.role, Role::Tool) {
            if !pending.remove(message.tool_call_id.as_deref()?) {
                return None;
            }
        } else if message.tool_call_id.is_some() {
            return None;
        }
        if pending.is_empty() {
            boundaries.push(index + 1);
            if let Some(start) = group_start.take() {
                groups.push(start);
            }
        }
    }
    let cut = cut_index(messages);
    if cut > 0 && boundaries.contains(&cut) {
        return Some(0..cut);
    }
    // Keep recent multi-turn exchanges verbatim. The fallback is strictly
    // for one real user turn, retaining its instruction and two full groups.
    let users: Vec<_> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_user_instruction(message))
        .map(|(index, _)| index)
        .collect();
    if users.len() != 1 {
        return None;
    }
    let start = users[0] + 1;
    let groups: Vec<_> = groups.into_iter().filter(|index| *index >= start).collect();
    if groups.len() <= 2 {
        return None;
    }
    let end = groups[groups.len() - 2];
    (boundaries.contains(&start) && boundaries.contains(&end)).then_some(start..end)
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
/// worth compacting: no safe range exists, the range is below
/// `MIN_TOKENS_TO_SUMMARIZE`, or the summary is empty or does not shrink
/// the measured history.
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
    let Some(range) = compaction_range(messages) else {
        return Ok(None);
    };
    if session_tokens(&messages[range.clone()]) < MIN_TOKENS_TO_SUMMARIZE {
        return Ok(None);
    }

    let messages_before = messages.len();
    let tokens_before = session_tokens(messages);

    let mut to_summarize = messages[..range.end].to_vec();
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

    let mut new_messages = messages[..range.start].to_vec();
    new_messages.push(Message::user(format!("{SUMMARY_PREFIX}{}", res.text)));
    new_messages.extend_from_slice(&messages[range.end..]);
    if session_tokens(&new_messages) >= tokens_before {
        return Ok(None);
    }
    if let Some(archive) = before_compact {
        archive(messages)?;
    }

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

    struct MeasuredSummary<'a>(&'a str);

    #[async_trait::async_trait]
    impl Provider for MeasuredSummary<'_> {
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

    #[tokio::test]
    async fn nonshrinking_summary_preserves_history_and_usage() {
        let mut messages = compactable_history();
        let original = serde_json::to_value(&messages).unwrap();
        struct Echo;
        #[async_trait::async_trait]
        impl Provider for Echo {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<polaris_provider::CompletionResponse, ProviderError> {
                Ok(polaris_provider::CompletionResponse {
                    text: req
                        .messages
                        .iter()
                        .map(|m| m.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    ..Default::default()
                })
            }
        }
        let archive = |_: &[Message]| -> std::io::Result<()> { panic!("must shrink first") };
        let outcome = compact_with_usage(&Echo, &mut messages, Some(&archive)).await;
        assert!(outcome.result.unwrap().is_none());
        assert_eq!(outcome.usage_report.missing_responses, 1);
        assert_eq!(serde_json::to_value(&messages).unwrap(), original);
    }

    #[tokio::test]
    async fn single_turn_retains_instruction_recent_groups_and_pending_call() {
        let mut messages = vec![user("Original instruction: keep all constraints")];
        for id in ["a", "b", "c", "d"] {
            messages.push(assistant_with_call("", id));
            messages.push(tool_result(id, &"x".repeat(MIN_TOKENS_TO_SUMMARIZE * 10)));
        }
        messages.push(assistant_with_call("still pending", "pending"));
        let original_user = serde_json::to_value(&messages[0]).unwrap();
        let tail = serde_json::to_value(&messages[5..]).unwrap();
        let report = compact(&Summarizer, &mut messages).await.unwrap().unwrap();
        assert!(report.tokens_after < report.tokens_before);
        assert_eq!(serde_json::to_value(&messages[0]).unwrap(), original_user);
        assert!(messages[1].content.starts_with(SUMMARY_PREFIX));
        assert_eq!(serde_json::to_value(&messages[2..]).unwrap(), tail);
        // A pending call prevents later groups from supplying a safe boundary.
        messages.push(assistant_with_call("", "later"));
        messages.push(tool_result("later", "done"));
        assert!(compaction_range(&messages).is_none());
    }

    #[test]
    fn cross_turn_pending_and_partially_completed_batches_are_not_cut() {
        let mut messages = compactable_history();
        messages.insert(1, assistant_with_call("", "pending"));
        assert!(compaction_range(&messages).is_none());
        let mut messages = vec![user("instruction")];
        let mut batch = assistant_with_call("", "a");
        batch
            .tool_calls
            .extend(assistant_with_call("", "b").tool_calls);
        messages.push(batch);
        messages.push(tool_result("a", "partial"));
        for id in ["c", "d", "e"] {
            messages.push(assistant_with_call("", id));
            messages.push(tool_result(id, "done"));
        }
        assert!(compaction_range(&messages).is_none());
        messages.push(tool_result("b", "done"));
        // All overlapping calls form one indivisible completed group.
        assert!(compaction_range(&messages).is_none());
    }

    #[tokio::test]
    async fn automatic_no_op_does_not_throttle_newly_eligible_history() {
        let meter = polaris_provider::UsageMeter::default();
        let measured = meter.wrap(&MeasuredSummary("summary"));
        let mut session = crate::session::Session::new();
        session.messages = compactable_history();
        session.messages.pop();
        let original = serde_json::to_value(&session.messages).unwrap();
        let tokens_before = session_tokens(&session.messages);
        assert!(
            session
                .compact_automatically(&measured)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(meter.snapshot().reported_responses, 0);
        assert_eq!(serde_json::to_value(&session.messages).unwrap(), original);
        assert!(session.compaction_retry_tokens.is_none());

        session.push_user("continue");
        assert!(
            session_tokens(&session.messages) - tokens_before
                < MIN_TOKENS_TO_SUMMARIZE.max(tokens_before / 10)
        );
        let report = session
            .compact_automatically(&measured)
            .await
            .unwrap()
            .unwrap();
        assert!(report.tokens_after < report.tokens_before);
        assert_eq!(meter.snapshot().reported_responses, 1);
        assert_eq!(session.messages.last().unwrap().content, "continue");
        assert!(session.compaction_retry_tokens.is_none());
    }

    #[tokio::test]
    async fn automatic_retry_waits_for_growth_and_meters_only_actual_attempts() {
        struct Failing;
        #[async_trait::async_trait]
        impl Provider for Failing {
            async fn complete(
                &self,
                _: CompletionRequest,
            ) -> Result<polaris_provider::CompletionResponse, ProviderError> {
                Err(ProviderError::Http("failed".into()))
            }
        }
        let nonshrinking = "summary ".repeat(MIN_TOKENS_TO_SUMMARIZE * 100);
        let nonshrinking_provider = MeasuredSummary(&nonshrinking);
        for provider in [
            &MeasuredSummary(" ") as &dyn Provider,
            &Failing,
            &nonshrinking_provider,
        ] {
            let meter = polaris_provider::UsageMeter::default();
            let measured = meter.wrap(provider);
            let mut session = crate::session::Session::new();
            session.messages = compactable_history();
            let _ = session.compact_automatically(&measured).await;
            let first = meter.snapshot();
            for _ in 0..5 {
                session.push_assistant("small", vec![]);
                assert!(
                    session
                        .compact_automatically(&measured)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            let unchanged = meter.snapshot();
            assert_eq!(unchanged.reported_responses, first.reported_responses);
            assert_eq!(unchanged.failed_requests, first.failed_requests);
            assert_eq!(unchanged.usage.total_tokens, first.usage.total_tokens);
            session.push_assistant(&"x".repeat(MIN_TOKENS_TO_SUMMARIZE * 20), vec![]);
            let _ = session.compact_automatically(&measured).await;
            let second = meter.snapshot();
            assert_eq!(second.reported_responses, first.reported_responses * 2);
            assert_eq!(second.failed_requests, first.failed_requests * 2);
            assert_eq!(second.usage.total_tokens, first.usage.total_tokens * 2);
        }
        let mut session = crate::session::Session::new();
        session.messages = compactable_history();
        assert!(
            session
                .compact_automatically(&Summarizer)
                .await
                .unwrap()
                .is_some()
        );
        assert!(session.compaction_retry_tokens.is_none());
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
