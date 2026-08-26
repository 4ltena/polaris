# polaris 会話履歴の自動圧縮(compaction) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `session.messages`が固定の実測トークン上限を超えたとき、直近の実ユーザーターンを残して古い履歴をLLMに要約させ置き換える。自動発火・手動`/compact`コマンド・TUI通知・`/resume`との整合まで含む。

**Architecture:** 新規`polaris-core::compaction`モジュール(トークン計測・発火判定・要約実行、`Provider`経由の1回のLLM呼び出し)を`agent::run_loop`の毎ターン、`provider.complete`の直前に組み込む。発火したら既存の`AgentEvent`チャンネルで`polaris-tui`へ通知し、TUI側は通知行を表示したうえで、ターン完了後(`session`への借用が安全な地点)に`persist::rewrite`で永続化ログを圧縮後の内容へ同期する。`/compact`スラッシュコマンドは同じ`compaction::compact`を閾値に関係なく直接呼ぶ。

**Tech Stack:** Rust、`polaris-core`(`compaction.rs`新規、`agent.rs`、`events.rs`、`budget.rs`を再利用)、`polaris-tui`(`render.rs`、`slash.rs`、`lib.rs`)。

**Spec:** `docs/superpowers/specs/2026-08-26-polaris-history-compaction-design.md`

## Global Constraints

- `COMPACTION_THRESHOLD: usize = 100_000`(トークン)、`KEEP_RECENT_USER_TURNS: usize = 2`。どちらも`crates/polaris-core/src/compaction.rs`の定数。
- 圧縮は必ず`Role::User`のメッセージ位置でしか切らない(ツール呼び出し/結果の対を割らない)。
- `run_loop`のシグネチャは変更しない——`provider`・`session`・`system`・`tools`・`events`は既存パラメータのみで足りる。
- TUI側で圧縮発生後に`session.messages`へ触れるのは、ターン完了後の既知安全な地点(`TurnOutcome::Done(Ok(result))`分岐、既存の`persist::append_message`呼び出しのすぐ隣)のみ。`events_rx`受信ループの中では触れない(借用チェッカーに落ちる)。
- 要約リクエスト自体が失敗した場合はエラーを`?`でそのまま伝播させる(握りつぶさない)。
- 各タスクの最後に該当crateの`cargo test`、最終タスクで`cargo test --workspace`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo fmt --all -- --check`を通す。
- コミットメッセージはConventional Commits形式、`Co-Authored-By: Claude <noreply@anthropic.com>`を末尾に付ける。

---

### Task 1: `compaction.rs`のコア(`polaris-core`)

**Files:**
- Create: `crates/polaris-core/src/compaction.rs`
- Modify: `crates/polaris-core/src/lib.rs`(モジュール宣言追加)

**Interfaces:**
- Produces: `pub const COMPACTION_THRESHOLD: usize`、`pub const KEEP_RECENT_USER_TURNS: usize`、`pub fn session_tokens(messages: &[Message]) -> usize`、`pub fn should_compact(total_tokens: usize) -> bool`、`pub struct CompactionReport { pub messages_before: usize, pub messages_after: usize, pub tokens_before: usize, pub tokens_after: usize }`、`pub async fn compact(provider: &dyn Provider, messages: &mut Vec<Message>) -> Result<Option<CompactionReport>, ProviderError>`。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全テストPASS(ベースライン確認)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-core/src/compaction.rs`を新規作成し、モジュール冒頭のdocコメントの下に`mod tests`を用意して以下を書く(実装本体はまだ無い状態):

```rust
//! Automatic history summarization. Fires when the conversation's measured
//! token count crosses a fixed ceiling, replacing everything before the
//! most recent few user turns with one LLM-generated summary message.

use crate::budget::count_tokens;
use polaris_provider::{CompletionRequest, Message, Provider, ProviderError, Role, ToolCall};

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

        let with_reasoning = session_tokens(&[Message::assistant("done")
            .with_reasoning(vec![polaris_provider::ReasoningItem {
                id: "r1".into(),
                encrypted_content: "x".repeat(1000),
            }])]);
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
        assert_eq!(cut_index(&exactly_two), 0, "exactly KEEP_RECENT_USER_TURNS turns — nothing to compact");

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

        let report = compact(&Summarizer, &mut messages).await.expect("should succeed");

        assert!(report.is_none());
        assert_eq!(messages.len(), original.len());
        assert_eq!(messages[0].content, original[0].content);
    }
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-core compaction`
Expected: コンパイルエラー(`session_tokens`・`should_compact`・`cut_index`・`compact`・`CompactionReport`が未定義)

- [ ] **Step 4: 実装する**

`compaction.rs`の`mod tests`の直前に追加:

```rust
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
```

`crates/polaris-core/src/lib.rs`の`mod dir_watch;`の直後(アルファベット順)に`mod compaction;`ではなく`pub mod compaction;`を追加する(`polaris-tui`が`/compact`コマンドで`compaction::compact`を直接呼ぶため`pub`が必要 — `events`のような`mod`+re-exportではなく、`session`/`stop`と同じ`pub mod`パターンにする):

```rust
pub mod compaction;
```

(既存の`mod dir_watch;`の直前か直後、アルファベット順の位置に挿入する — 現在の並びは `agent, approval, audit, budget, config, constitution, dir_watch, events, files_md, gitignore, project, prompt, secret_screen, session, spawn, stop` なので `budget` と `config` の間ではなく、`c`で始まる既存の`config`/`constitution`との順序を保って挿入する: `compaction` は `budget` の直後・`config` の直前)

- [ ] **Step 5: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全テストPASS

- [ ] **Step 6: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-core -- --check && cargo clippy -p polaris-core --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-core/src/compaction.rs crates/polaris-core/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(polaris-core): add the history-compaction core module

session_tokens/should_compact/cut_index/compact — a fixed
model-agnostic token ceiling, cutting only at Role::User boundaries so
tool-call/result pairs never split, and a single LLM-summarization
call replacing everything older than the kept recent turns. Not yet
wired into run_loop.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `run_loop`への組み込みと`AgentEvent::HistoryCompacted`

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`
- Modify: `crates/polaris-core/src/events.rs`

**Interfaces:**
- Consumes: `compaction::{session_tokens, should_compact, compact}`(Task 1)。
- Produces: `AgentEvent::HistoryCompacted { messages_before: usize, messages_after: usize, tokens_before: u32, tokens_after: u32 }`。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全テストPASS(ベースライン確認、Task 1込み)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs`の`mod tests`内、`reasoning_from_a_tool_calling_turn_is_carried_into_the_session`の直後に追加する(`Scripted`プロバイダを再利用):

```rust
#[tokio::test]
async fn a_turn_that_crosses_the_compaction_threshold_compacts_before_sending() {
    let dir = tempfile::tempdir().expect("temp directory");

    // Two big prior turns (well past COMPACTION_THRESHOLD once summed),
    // then a small final turn that triggers compaction before it sends.
    let big = "x".repeat(polaris_core::compaction::COMPACTION_THRESHOLD * 5); // chars, not tokens, but plenty either way
    let mut session = Session::new();
    session.push_user("turn 1");
    session.push_assistant(&big, vec![]);
    session.push_user("turn 2");
    session.push_assistant("reply 2", vec![]);
    session.push_user("turn 3 — the new message that pushes it over");

    let p = Scripted {
        replies: Mutex::new(vec![
            // The summarization call compact() makes internally.
            CompletionResponse {
                text: "summary of turns before the kept tail".into(),
                ..Default::default()
            },
            // The real turn's own response, sent after compaction.
            CompletionResponse {
                text: "final reply".into(),
                ..Default::default()
            },
        ]),
    };

    let audit = shared_audit(&dir.path().join("audit.jsonl"));
    let mut stop = StopTracker::new(10);
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let always_on = crate::prompt::assemble_always_on("", "", &[]);
    let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper: &helper,
        gate: &mut gate,
        approver: &mut approver,
    };

    let outcome = run(
        &p,
        &mut session,
        audit.clone(),
        &mut stop,
        &always_on,
        &[],
        &[],
        unused_provider_pool(),
        crate::spawn::DEFAULT_CONCURRENCY,
        crate::spawn::DEFAULT_WRITE_CONCURRENCY,
        Some(events_tx),
        &mut ctx,
    )
    .await
    .expect("should succeed");

    assert_eq!(outcome.text, "final reply");

    let event = events_rx.try_recv().expect("a HistoryCompacted event should have been sent");
    assert!(
        matches!(event, polaris_core::AgentEvent::HistoryCompacted { .. }),
        "expected HistoryCompacted, got {event:?}"
    );

    // session.messages was replaced: 1 summary + the kept tail (turn 2,
    // reply 2, turn 3) + the final assistant reply = 5.
    assert_eq!(session.messages.len(), 5);
    assert!(session.messages[0].content.contains("summary of turns before the kept tail"));
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-core a_turn_that_crosses_the_compaction_threshold_compacts_before_sending`
Expected: コンパイルエラー(`AgentEvent::HistoryCompacted`が存在しない、`polaris_core::compaction`が未`pub`——Task 1で対応済みのはずだが、`run_loop`が閾値チェックを行っていないため`Scripted`の返信順序が合わずテストが失敗する)

- [ ] **Step 4: `AgentEvent::HistoryCompacted`を追加する**

`crates/polaris-core/src/events.rs`の`pub enum AgentEvent`、`SpawnFinished { ... }`の直後に追加:

```rust
    HistoryCompacted {
        messages_before: usize,
        messages_after: usize,
        tokens_before: u32,
        tokens_after: u32,
    },
```

- [ ] **Step 5: `run_loop`に発火チェックを組み込む**

`crates/polaris-core/src/agent.rs`の`run_loop`、`let res = provider.complete(...)`の直前(agent.rs:319付近)に挿入:

```rust
        let total_tokens =
            crate::budget::always_on_tokens(system, tools) + crate::compaction::session_tokens(&session.messages);
        if crate::compaction::should_compact(total_tokens)
            && let Some(report) = crate::compaction::compact(provider, &mut session.messages).await?
        {
            if let Some(tx) = &events {
                let _ = tx.send(crate::events::AgentEvent::HistoryCompacted {
                    messages_before: report.messages_before,
                    messages_after: report.messages_after,
                    tokens_before: report.tokens_before as u32,
                    tokens_after: report.tokens_after as u32,
                });
            }
        }

        let res = provider
            .complete(CompletionRequest {
```

(直後の既存の`.complete(CompletionRequest { ... })`ブロックはそのまま変更しない)

- [ ] **Step 6: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全テストPASS

- [ ] **Step 7: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-core -- --check && cargo clippy -p polaris-core --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 8: コミット**

```bash
git add crates/polaris-core/src/agent.rs crates/polaris-core/src/events.rs
git commit -m "$(cat <<'EOF'
feat(polaris-core): fire compaction from run_loop, add its AgentEvent

Every turn, before calling the provider, measures
always_on_tokens + compaction::session_tokens against the fixed
threshold. When it fires, compact() runs first and an
AgentEvent::HistoryCompacted goes out on the existing events channel
so the TUI (and any other consumer) can react - no run_loop signature
change needed, since provider/session/system/tools/events were all
already parameters.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: TUI表示(`render.rs`)

**Files:**
- Modify: `crates/polaris-tui/src/render.rs`

**Interfaces:**
- Consumes: `AgentEvent::HistoryCompacted`(Task 2)。
- Produces: `format_event_for_live_print`が`HistoryCompacted`を処理する。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-tui`
Expected: 全テストPASS(ベースライン確認。Task 1・2は別crateなのでこの時点ではまだ`cargo build --workspace`できない — `AgentEvent`の`match`が新しいvariantを認識できずコンパイルエラーになるはず。次のStepで確認する)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-tui/src/render.rs`の`mod tests`内、`a_finished_spawn_reports_done_or_failed`(またはSpawnFinished関連の直近のテスト)の直後に追加する:

```rust
#[test]
fn a_history_compacted_event_reports_before_and_after_counts() {
    let joined = live_print_text(&AgentEvent::HistoryCompacted {
        messages_before: 12,
        messages_after: 3,
        tokens_before: 48_201,
        tokens_after: 2_103,
    });
    assert!(joined.contains("12"));
    assert!(joined.contains('3'));
    assert!(joined.contains("48"));
    assert!(joined.contains("2,103") || joined.contains("2103"));
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-tui a_history_compacted_event_reports_before_and_after_counts`
Expected: コンパイルエラー(`format_event_for_live_print`の`match`が`AgentEvent::HistoryCompacted`を網羅していない——`polaris-core`側でこのvariantが追加済みなら`non-exhaustive match`エラーになる)

- [ ] **Step 4: `format_event_for_live_print`に分岐を追加する**

`crates/polaris-tui/src/render.rs`の`format_event_for_live_print`内、`AgentEvent::SpawnFinished { agent_type, ok } => { ... }`の直後・`match`を閉じる`}`の直前に追加:

```rust
        AgentEvent::HistoryCompacted {
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
        } => {
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!(
                    "⏺ 会話履歴を要約しました ({messages_before}件→{messages_after}件、{tokens_before}tok→{tokens_after}tok)"
                )),
                Style::default().add_modifier(Modifier::DIM),
            )))]
        }
```

- [ ] **Step 5: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tui`
Expected: 全テストPASS

- [ ] **Step 6: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-tui -- --check && cargo clippy -p polaris-tui --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-tui/src/render.rs
git commit -m "$(cat <<'EOF'
feat(polaris-tui): render a notice line for HistoryCompacted

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `/compact`スラッシュコマンドの定義(`slash.rs`)

**Files:**
- Modify: `crates/polaris-tui/src/slash.rs`
- Modify: `crates/polaris-tui/src/lib.rs`(`apply_slash_action`の1箇所のみ、下記Step 4bを参照)

**Interfaces:**
- Produces: `slash::Action::Compact`、`slash::COMMANDS`に`"compact"`エントリ追加、`action_for("compact")`が`Action::Compact`を返す。

**事前確認済みの事実**: `crates/polaris-tui/src/lib.rs`の`apply_slash_action`内の`match action { ... }`は、`Action`の全variantをワイルドカード無しで網羅している——`Action::Review(_) | Action::New | Action::Resume | Action::Permissions | Action::Fork | Action::Model | Action::Skills => { ... }`という1本のアームが、「`run()`側で割り込み処理されるため実際にはここへ到達しないvariant」をまとめて受けている(到達しても`SlashOutcome::Continue`を返すだけの安全なフォールバック)。`Action::Compact`もこれと同じ「`run()`側で割り込み処理される」性質を持つため、このアームに`| slash::Action::Compact`を追加するだけで良い。これを追加し忘れると`cargo build -p polaris-tui`自体が`Action::Compact`分岐が無いという理由でコンパイルエラーになる(ワイルドカード無しの網羅的matchのため)。`run()`本体側の実際の割り込み処理(`handle_compact`の呼び出し)はTask 5で追加する——Task 4の時点では「到達しても安全なフォールバックに含める」だけで、`cargo build`を通すのに十分。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-tui slash`
Expected: 全テストPASS

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-tui/src/slash.rs`のテストモジュール内、`clear`関連の既存テスト(例: `parse("/clear")`をアサートしているテスト)の近くに追加する:

```rust
#[test]
fn compact_is_a_known_command() {
    assert!(matches!(parse("/compact"), Some(Action::Compact)));
    assert!(matches!(action_for("compact"), Action::Compact));
    assert!(COMMANDS.iter().any(|c| c.name == "compact"));
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-tui compact_is_a_known_command`
Expected: コンパイルエラー(`Action::Compact`が存在しない)

- [ ] **Step 4: `Action::Compact`を追加する**

`crates/polaris-tui/src/slash.rs`の`pub enum Action`、`Clear,`の直後に追加:

```rust
    /// Not handled by `apply_slash_action` — needs a real `&dyn Provider`
    /// reference and an async LLM call, which `apply_slash_action` (sync,
    /// display-strings-only) doesn't have. The caller in `lib.rs`
    /// intercepts this the same way it does `Model`.
    Compact,
```

`pub const COMMANDS`配列、`clear`エントリの直後に追加:

```rust
    SlashCommand {
        name: "compact",
        description: "summarize older history now to free up context",
    },
```

`action_for`内の`match`、`"clear" => Action::Clear,`の直後に追加:

```rust
        "compact" => Action::Compact,
```

ファイル冒頭のdocコメント(1-27行目)にある「polaris が持たないため見送った」コマンド一覧から`/compact`を削除する(実装したため、もう見送りリストに含めるのは不正確):

```
- 変更前: `/mcp`, `/apps`, `/plugins`, `/compact`, `/vim`, ...
- 変更後: `/mcp`, `/apps`, `/plugins`, `/vim`, ...
```

- [ ] **Step 4b: `apply_slash_action`のフォールバックアームに加える**

`crates/polaris-tui/src/lib.rs`の`apply_slash_action`内、`slash::Action::Review(_)`から始まる複数行にわたるフォールバックアーム(`| slash::Action::Skills => {`で終わる行)に`| slash::Action::Compact`を追加する:

```rust
        slash::Action::Review(_)
        | slash::Action::New
        | slash::Action::Resume
        | slash::Action::Permissions
        | slash::Action::Fork
        | slash::Action::Model
        | slash::Action::Skills
        | slash::Action::Compact => {
```

(このアームの中身・戻り値は変更しない——追加するのはパターンの1行だけ)

- [ ] **Step 5: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tui`
Expected: 全テストPASS

- [ ] **Step 6: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-tui -- --check && cargo clippy -p polaris-tui --all-targets -- -D warnings`
Expected: どちらもクリーン(Step 4bにより`cargo build -p polaris-tui`もこの時点で通る)

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-tui/src/slash.rs crates/polaris-tui/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(polaris-tui): add the /compact slash command

Parsing only, plus the one-line fallback-arm addition
apply_slash_action's exhaustive match requires. apply_slash_action
itself doesn't have provider access to actually run compaction (that
needs an async LLM call) - lib.rs's run() intercepts this action the
same way it already does /model. Real wiring lands in the next commit.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: TUI配線(`lib.rs`) — `handle_compact`・通知・永続化再同期

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `slash::Action::Compact`(Task 4)、`compaction::compact`(Task 1、`polaris_core::compaction::compact`として)、`AgentEvent::HistoryCompacted`(Task 2)、`persist::rewrite`(既存)。

このタスクの前に、`crates/polaris-tui/src/lib.rs`を実際に読んで以下を確認すること(このplanを書いた時点の行番号はズレている可能性がある):
- `pub async fn run(args: RunArgs<'_>) -> ExitCode`内、`slash::Action::Model => { handle_model(...); continue; }`の位置(このplan作成時点でlib.rs:779-789)。
- `events_rx.recv()`の受信ループ(このplan作成時点でlib.rs:1267付近)と、ターン終了直前の`while let Ok(event) = events_rx.try_recv()`ドレイン(このplan作成時点でlib.rs:1541付近)。
- `TurnOutcome::Done(Ok(result)) => { ... persist::append_message(&session_path, reply) ... }`の位置(このplan作成時点でlib.rs:1551-1562)。

**既知のテスト範囲の限界**: `run()`本体(TUIのメインループ)を実際に駆動する既存のテストハーネスが無い(fake terminal・fake key readerを組み立てる仕組みがこのcrateに存在しない)。そのため、mid-turnの自動発火経路(`events_rx`でのフラグ立て→ターン完了後の`persist::rewrite`)そのものを自動テストで直接検証することはこのタスクのscopeでは行わない——`handle_compact`自体(Step 4で追加する、独立した関数)は単体テストする(Step 2-3)。自動発火経路はStep 10の実地確認とコードレビューに委ねる。spec のテスト方針7番はこの限界の範囲内で満たす。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-tui`
Expected: 全テストPASS(ベースライン確認、Task 4込み)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-tui/src/lib.rs`の`mod tests`内(`use polaris_provider::Message;`の直後あたり)に、`compaction.rs`のTask 1で使った`Summarizer`と同じ形のモックプロバイダを追加し、`handle_compact`のテストを書く:

```rust
    struct Summarizer;

    #[async_trait::async_trait]
    impl polaris_provider::Provider for Summarizer {
        async fn complete(
            &self,
            _req: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            Ok(polaris_provider::CompletionResponse {
                text: "summary text".to_string(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn handle_compact_compacts_and_persists_when_there_is_something_to_compact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("s.jsonl");

        let mut session = Session::default();
        session.push_user("turn 1");
        session.push_assistant("reply 1", vec![]);
        session.push_user("turn 2");
        session.push_assistant("reply 2", vec![]);
        session.push_user("turn 3");
        session.push_assistant("reply 3", vec![]);

        let mut status = Status::Idle;
        handle_compact(&Summarizer, &mut session, &session_path, &mut status).await;

        assert!(
            matches!(&status, Status::Notice(s) if s.contains("compacted")),
            "expected a 'compacted' notice, got a different status"
        );
        assert_eq!(session.messages.len(), 5, "1 summary + kept tail of 4");

        let (loaded, _truncated) = persist::load_session(&session_path).expect("load");
        assert_eq!(
            loaded.messages.len(),
            session.messages.len(),
            "the persisted file must reflect the compacted session, not the pre-compaction one"
        );
        assert_eq!(loaded.messages[0].content, session.messages[0].content);
    }

    #[tokio::test]
    async fn handle_compact_reports_when_there_is_nothing_to_compact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("s.jsonl");
        let mut session = Session::default();
        session.push_user("only turn");
        let mut status = Status::Idle;

        handle_compact(&Summarizer, &mut session, &session_path, &mut status).await;

        assert!(
            matches!(&status, Status::Notice(s) if s.contains("nothing")),
            "expected a 'nothing to compact' notice"
        );
        assert_eq!(session.messages.len(), 1, "must not have been touched");
    }
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-tui handle_compact`
Expected: コンパイルエラー(`handle_compact`が未定義)

- [ ] **Step 4: `handle_compact`関数を追加する**

`fn handle_model`の直後に追加(`handle_model`は同期関数だが、`handle_compact`はプロバイダへの非同期呼び出しを行うため`async fn`にする):

```rust
/// Runs compaction unconditionally (ignores the threshold — that's the
/// point of a manual command) and reports the result via `status`. On
/// success, also resyncs the persisted session log so `/resume` reflects
/// the compacted state, matching how `TurnOutcome::Done(Ok(_))` does the
/// same thing after an automatic compaction (see the call site in `run`).
async fn handle_compact(
    provider: &dyn Provider,
    session: &mut polaris_core::session::Session,
    session_path: &std::path::Path,
    status: &mut Status,
) {
    match polaris_core::compaction::compact(provider, &mut session.messages).await {
        Ok(None) => {
            *status = Status::Notice("nothing old enough to compact yet".to_string());
        }
        Ok(Some(report)) => {
            match persist::rewrite(session_path, &session.messages) {
                Ok(()) => {
                    *status = Status::Notice(format!(
                        "compacted {} messages → {} ({} tok → {} tok)",
                        report.messages_before,
                        report.messages_after,
                        report.tokens_before,
                        report.tokens_after
                    ));
                }
                Err(e) => {
                    *status = Status::Notice(format!(
                        "compacted in memory, but couldn't persist it: {e}"
                    ));
                }
            }
        }
        Err(e) => {
            *status = Status::Notice(format!("compaction failed: {e}"));
        }
    }
}
```

- [ ] **Step 5: `/compact`のディスパッチを追加する**

`run()`内、`slash::Action::Skills => { ... continue; }`の直後(捕捉アーム`action => { ... apply_slash_action(...) }`の直前)に追加:

```rust
                        slash::Action::Compact => {
                            handle_compact(
                                args.provider.as_ref(),
                                &mut session,
                                &session_path,
                                &mut status,
                            )
                            .await;
                            continue;
                        }
```

- [ ] **Step 6: 自動発火時の通知フラグを追加する**

`run()`内、ターンごとの状態(`turn_started`等)を初期化している箇所の近くに`let mut history_compacted_this_turn = false;`を追加する(ターンの外側スコープ——1回のユーザー送信〜応答完了まで生存する必要がある)。

`events_rx.recv()`の受信ループ(lib.rs:1267付近)、`let appended = append_live_event(&event, &mut history);`の直後に追加:

```rust
                        if matches!(event, polaris_core::AgentEvent::HistoryCompacted { .. }) {
                            history_compacted_this_turn = true;
                        }
```

ターン終了直前の`while let Ok(event) = events_rx.try_recv()`ドレイン(lib.rs:1541付近)、同様に`let appended = append_live_event(&event, &mut history);`の直後に同じ3行を追加する。

- [ ] **Step 7: ターン完了後の永続化分岐を書き換える**

`TurnOutcome::Done(Ok(result))`の分岐(lib.rs:1551-1562)を次のように変更する:

```rust
            TurnOutcome::Done(Ok(result)) => {
                cumulative_usage.input_tokens += result.usage.input_tokens;
                cumulative_usage.output_tokens += result.usage.output_tokens;
                cumulative_usage.total_tokens += result.usage.total_tokens;
                cumulative_usage.cached_tokens += result.usage.cached_tokens;
                status = Status::Idle;
                if history_compacted_this_turn {
                    if let Err(e) = persist::rewrite(&session_path, &session.messages) {
                        fatal_message = Some(format!("Can't persist the compacted session: {e}"));
                        break 'outer ExitCode::FAILURE;
                    }
                } else if let Some(reply) = session.messages.last()
                    && let Err(e) = persist::append_message(&session_path, reply)
                {
                    fatal_message = Some(format!("Can't persist the reply: {e}"));
                    break 'outer ExitCode::FAILURE;
                }
                history_compacted_this_turn = false;
            }
```

(`history_compacted_this_turn`はターンごとにリセットする——次のターンの誤判定を防ぐため、このブロックの最後で`false`に戻す。他の`TurnOutcome`分岐(`Done(Err(_))`等)ではリセットしない——次のターンでも圧縮済みのままの可能性があり、次に成功したときにまとめて永続化されれば十分という設計上の割り切り、spec参照)

- [ ] **Step 8: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tui`
Expected: 全テストPASS

- [ ] **Step 9: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-tui -- --check && cargo clippy -p polaris-tui --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 10: 実地確認(tmux)**

`docs/superpowers/CURRENT.md`にある通り、このプロジェクトはTUI変更を実際のターミナルで確認する慣行がある。tmuxで`./target/release/polaris`(要`cargo build --release`)を起動し、`/compact`を打って「圧縮するものがありません」の案内が出ることを確認する(閾値未満の通常の短い会話では自動発火しないため、手動コマンドの応答確認がこの時点での現実的な検証になる)。

- [ ] **Step 11: コミット**

```bash
git add crates/polaris-tui/src/lib.rs crates/polaris-tui/src/slash.rs
git commit -m "$(cat <<'EOF'
feat(polaris-tui): wire /compact and auto-compaction notice/persist

handle_compact runs compaction unconditionally and reports the result
via status. Automatic mid-turn compaction is tracked via a per-turn
flag (events_rx can't touch session directly while agent_future holds
it borrowed) and resynced to disk once the turn resolves and session
is safe to read again - the same point the existing
persist::append_message call already proves safe.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: ワークスペース全体の最終検証

**Files:** なし(検証・docs更新のみ)

- [ ] **Step 1: ワークスペース全体のテストを実行する**

Run: `cargo test --workspace`
Expected: 全テストPASS

- [ ] **Step 2: 静的解析を実行する**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: クリーン

- [ ] **Step 3: フォーマットを確認する**

Run: `cargo fmt --all -- --check`
Expected: クリーン

- [ ] **Step 4: `docs/filemap.md`のドリフトを確認・再生成する**

Run: `cargo test -p polaris-core --test filemap`

失敗した場合(このplan・specファイル自体が新規追加のため、まず間違いなく失敗する): `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`で再生成し、差分がこのタスクで追加したファイルの分だけであることを確認してからステージする。

- [ ] **Step 5: 実地確認(任意、実施した場合は記録する)**

`COMPACTION_THRESHOLD`を一時的に小さい値(例: 500)に書き換えたデバッグビルドで、実際に数往復するタスクを`exec`または対話TUIで流し、自動圧縮が発火して`AgentEvent::HistoryCompacted`の通知行が表示されることを確認する。確認後、`COMPACTION_THRESHOLD`は元の値に戻す(コミットしない)。

- [ ] **Step 6: `docs/superpowers/CURRENT.md`を更新する**

この機能が実装・検証済みであることを記録する。`docs/superpowers/CURRENT.md`にすでにある「reasoning item保持の最終レビューで未対応のまま残した2件」のうち、コンパクション不在に関する項目(コンテキスト増分の実測)がこの実装によって解消されたことも明記する。

- [ ] **Step 7: コミット**

```bash
git add docs/superpowers/CURRENT.md docs/filemap.md
git commit -m "$(cat <<'EOF'
docs: record history compaction as implemented and verified

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```
