# polaris 会話履歴の自動圧縮(compaction) 設計書

## 背景・目的

`crates/polaris-core/src/session.rs`は冒頭のコメントで明言している通り「M1、追記専用——圧縮なし、ディスク永続化なし」のまま今日まで来ている。`agent::run_loop`は毎ターン`session.messages`をそのまま`clone()`してプロバイダへ送るだけで(`crates/polaris-core/src/agent.rs:319-325`)、会話が伸びるほどリクエストは際限なく肥大化する。今回のreasoning item保持機能(同日実装)はこれをさらに悪化させた——`encrypted_content`という数KBの不透明blobが、ツール呼び出しを伴うターンごとに1件ずつ積み増され、二度と落ちない。

この欠落は、polarisとcodexで同一タスクを比較したレポート(利用者による実測、2026-08-26)で「codexが最大の技術リスクとして名指しした」項目でもある。upstream `codex-rs`にはこれに対応する圧縮機構が存在し、polarisには無い。「polarisがcodexに対して明確に優位でなければ普及しない」という利用者の方針のもと、この欠落を埋める。

upstream `codex-rs`の圧縮実装(`core/src/`配下、圧縮関連だけで10ファイル4,251行)は事前調査により確認済みだが、4戦略のfeature-flag切替、サーバー側圧縮エンドポイント連携、モデル降格時の処理、world-state/hookシステムなど、upstream自身のアーキテクチャに強く結びついた作りで、polarisの規模にはそのまま持ち込めない。本設計は、upstreamの発想(「古い履歴をLLMに要約させて置き換える」「直近の実ユーザーターンは残す」)だけを取り、実装は自己完結した最小限の1本の戦略にする。

## スコープ

対象: `polaris-core`(新規`compaction.rs`、`agent.rs`の`run_loop`、`events.rs`)、`polaris-tui`(圧縮イベントの表示、永続化ログの再同期、`/compact`コマンド)。

非対象:
- モデルごとのコンテキストウィンドウサイズの動的取得。固定の保守的なトークン上限を使う(ブレインストーミングで確定済み)。
- 個々の巨大なツール結果1件だけを縮める仕組み(upstreamの`trim_function_call_history_to_fit_context_window`相当)。圧縮後の「直近ターン」だけで固定上限を超えるような病的なケースへの対策は今回のscope外とし、既知の限界として残す(下記「見送った代替案」参照)。
- `polaris-cli`(一発実行)固有の対応。圧縮機構自体は`run_loop`に置くため、TUI・exec両方に自動的に効く。永続化(`persist.rs`)はTUI層にしか存在しないため、そちらだけが再同期を必要とする。
- サーバー側の要約API・複数戦略のfeature-flag切替。1本の「ローカルでLLMに要約させる」戦略のみ。

## アーキテクチャ

```
run_loop の毎ターン、provider.complete() を呼ぶ直前:

  総トークン数 = budget::always_on_tokens(system, tools)
               + compaction::session_tokens(&session.messages)

  総トークン数 >= COMPACTION_THRESHOLD なら:
    compaction::compact(provider, &mut session.messages) を実行
      → 直近 KEEP_RECENT_USER_TURNS 件のユーザーターンより前を
        1回のLLM呼び出しで要約し、その要約1件のMessageで置き換える
      → AgentEvent::HistoryCompacted を events チャンネルへ送出

  (圧縮の有無に関わらず) 縮んだ(かもしれない) session.messages で
  provider.complete() を呼ぶ

TUI側 (events_rx 受信ループ):
  AgentEvent::HistoryCompacted を受け取ったら
    → render::format_event_for_live_print が通知行を生成(既存の仕組み)
    → persist::rewrite(&session_path, &session.messages) で
      永続化ログを圧縮後の内容に合わせて丸ごと書き換える
```

### 1. `compaction.rs`(新規、`crates/polaris-core/src/`)

```rust
//! Automatic history summarization. Fires when the conversation's measured
//! token count crosses a fixed ceiling, replacing everything before the
//! most recent few user turns with one LLM-generated summary message.
```

定数:

```rust
/// Conservative and model-agnostic — polaris has no per-model context
/// window table (no provider exposes one), so this is picked well below
/// the smallest context window in common use (128k+) rather than tuned to
/// any specific model.
pub const COMPACTION_THRESHOLD: usize = 100_000;

/// How many of the most recent user turns survive compaction verbatim.
pub const KEEP_RECENT_USER_TURNS: usize = 2;
```

トークン計測(`budget.rs`の「実測する、推定しない」方針に倣う):

```rust
/// Sums `budget::count_tokens` over every `Message`'s content, its
/// tool_calls (in the same JSON shape actually sent on the wire), and its
/// reasoning items' encrypted_content — the same bytes that ride the wire
/// on replay, even though the content itself is opaque.
pub fn session_tokens(messages: &[Message]) -> usize
```

```rust
pub fn should_compact(total_tokens: usize) -> bool {
    total_tokens >= COMPACTION_THRESHOLD
}
```

圧縮点の決定(ターン境界を跨がない。ツール呼び出し/結果の対を割ってはならないため、`Role::User`のメッセージ位置でしか切らない):

```rust
/// Returns the index to cut at: the index of the `KEEP_RECENT_USER_TURNS`
/// most recent `Role::User` messages' *earliest* one — i.e. where the kept
/// tail begins. Example: `[user1, asst1, user2, asst2, user3, asst3]` with
/// `KEEP_RECENT_USER_TURNS = 2` returns the index of `user2` (turns 2 and 3
/// are kept; turn 1 and everything before it gets compacted away). Returns
/// 0 (nothing to compact) when there are `KEEP_RECENT_USER_TURNS` or fewer
/// user turns total — cutting at index 0 would compact an empty prefix,
/// which is a no-op, so 0 doubles as the sentinel for "nothing to do."
fn cut_index(messages: &[Message]) -> usize
```

実行:

```rust
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
) -> Result<Option<CompactionReport>, ProviderError>
```

`compact`の内部処理:
1. `cut_index(messages)`を求める。0なら`Ok(None)`で終わる。
2. `messages[..cut]`をそのまま`CompletionRequest`の`messages`とし、末尾に要約依頼の`Message::user(SUMMARIZE_INSTRUCTION)`を追加、`system`は圧縮専用の短い固定文、`tools: vec![]`(要約にツール呼び出しは不要)で`provider.complete(...)`を呼ぶ。
3. 返ってきた`res.text`を`format!("{SUMMARY_PREFIX}{}", res.text)`で包み、`Message::user(...)`として1件だけの新しい先頭Messageを作る。
4. `*messages = vec![要約Message]; messages.extend_from_slice(&元のmessages[cut..]);`
5. 圧縮前後の件数・トークン数を`CompactionReport`として返す。

プロンプト文言:

```rust
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
```

要約専用リクエストの`system`はこの固定文で足りる(要約というタスク自体に、通常ターンのconstitution/environment/skillsは不要)。

### 2. `run_loop`への組み込み(`crates/polaris-core/src/agent.rs`)

`provider.complete(...)`を呼ぶ直前(agent.rs:319付近)に挿入:

```rust
let total_tokens = crate::budget::always_on_tokens(system, tools)
    + crate::compaction::session_tokens(&session.messages);
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
```

`compact`が失敗した場合(要約リクエスト自体がネットワーク/プロバイダエラーで失敗した場合)は`?`でそのまま`run_loop`のエラーとして伝播させる—圧縮は事前予防のベストエフォートだが、失敗を握りつぶして肥大化したまま本来のリクエストを送ると、そちらもいずれ同じ理由で失敗する可能性が高く、握りつぶす利点が薄いため。

`run_loop`のsubagent呼び出し(`spawn.rs`経由)にもこの分岐は同じ形で効く——subagentも同じ`run_loop`を通るため、専用の配線は不要。

### 3. `AgentEvent::HistoryCompacted`(`crates/polaris-core/src/events.rs`)

```rust
pub enum AgentEvent {
    // 既存の ToolStarted / ToolFinished / SpawnStarted / SpawnFinished ...
    HistoryCompacted {
        messages_before: usize,
        messages_after: usize,
        tokens_before: u32,
        tokens_after: u32,
    },
}
```

### 4. TUI側の表示(`crates/polaris-tui/src/render.rs`)

`format_event_for_live_print`(既存、`AgentEvent`ごとに`HistoryLine`を組み立てている関数)へ`HistoryCompacted`の分岐を追加し、通知行を1行返す(例: `"⏺ 会話履歴を要約しました (12件→3件、48,201tok→2,103tok)"`)。既存の`ToolStarted`等と同じ表示レーンに乗る。

### 5. TUI側の永続化再同期・`/compact`コマンド(`crates/polaris-tui/src/lib.rs`)

`events_rx`受信ループ(lib.rs:1267)の時点では`agent_future`が`session`を可変借用中のため、その場で`session.messages`へは触れられない(借用チェッカーに落ちる)。そこで直接`persist::rewrite`は呼ばず、ターンの外側スコープに`let mut history_compacted_this_turn = false;`を用意し、`events_rx`受信ループ・ターン終了直前のtry_recvドレイン(lib.rs:1541)の両方で、受け取ったイベントが`AgentEvent::HistoryCompacted`のときだけこのフラグを立てる(`append_live_event`自体は変更しない——既存通りイベント種別を問わず呼ぶ):

```rust
if matches!(event, polaris_core::AgentEvent::HistoryCompacted { .. }) {
    history_compacted_this_turn = true;
}
```

ターンが完了し`session`へ安全に触れられる`TurnOutcome::Done(Ok(result))`の分岐(lib.rs:1551、既存の`persist::append_message`呼び出しのすぐ隣)で、このフラグを見て分岐する:

```rust
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
```

`rewrite`は`session.messages`全体(圧縮後の要約+直近ターン+今回の応答)を書き込むため、通常の`append_message`と同時には呼ばない。既存の`/clear`が`persist::clear_session`を呼ぶのと同じ「全体書き換え」の扱い。`polaris-cli`(exec)はそもそも永続化しないため、この呼び出しはTUI側だけに置く。

既知の限界: `TurnOutcome::Done(Err(e))`等、圧縮の直後にターン自体が失敗した経路では`history_compacted_this_turn`を見ていない——ディスク上のログは次に成功するターンか、明示的な`/compact`・`/clear`まで圧縮前の内容のまま残る。メモリ上の`session`自体は正しく圧縮済みのため実害は無いが、その間に`/resume`すると圧縮前の状態が読み込まれる。今回のscopeでは許容する。

新規スラッシュコマンド`/compact`: 既存のコマンド分岐に追加し、上限に関係なく`compaction::compact(...)`を直接呼ぶ。閾値未満で呼んだ場合(`cut_index`が0を返す=直近ターンしかない)は「圧縮するものがありません」のような案内を出す。

## エラー処理

- 要約リクエスト自体の失敗は`run_loop`の通常のターン失敗として伝播する(上記)。
- `cut_index`が0(直近`KEEP_RECENT_USER_TURNS`件のユーザーターンしか無い、またはユーザーターンが1件も無い)ときは`compact`は何もせず`Ok(None)`を返す——エラーではない。
- 圧縮後、直近ターンだけで再び`COMPACTION_THRESHOLD`を超えている病的なケース(1件のツール結果が極端に大きい等)は、次のターンでも`should_compact`が真になり続けるが、`cut_index`が0を返すため無限ループはしない(圧縮対象が無ければ`compact`は無条件で`Ok(None)`)——ただしこの場合、肥大化した履歴のままプロバイダへ送り続けることになる。個々の巨大メッセージを縮める機構(upstreamの`trim_function_call_history_to_fit_context_window`相当)は今回のscope外(上記「非対象」参照)。

## テスト方針

1. `session_tokens`: content・tool_calls・reasoningそれぞれを持つMessageの組み合わせで、期待通りの合計になること。
2. `should_compact`: 閾値ちょうど・閾値未満・閾値超過の境界値テスト。
3. `cut_index`: ユーザーターンが`KEEP_RECENT_USER_TURNS`件未満のとき0を返すこと、ちょうどのとき0を返すこと(切るものが無い)、それより多いとき正しい位置(ツール呼び出し/結果の対を割らない、Role::Userの位置)を返すこと。
4. `compact`(`Scripted`風のモックプロバイダを使用): 要約が正しく先頭1件のMessageに置き換わり、直近ターンがそのまま残ること。要約リクエストの`messages`に圧縮対象の範囲が正しく渡っていること。圧縮対象が無いとき`Ok(None)`を返し`messages`が変化しないこと。
5. `run_loop`統合テスト: 閾値を超える大きさのMessageを事前に積んだ`Session`で`run`を呼び、`compact`が呼ばれて`session.messages`が縮むこと、`AgentEvent::HistoryCompacted`が送出されること。
6. `format_event_for_live_print`: `HistoryCompacted`から正しい内容の`HistoryLine`が生成されること。
7. TUI統合(`lib.rs`): `HistoryCompacted`受信時に`persist::rewrite`が呼ばれ、ファイルの中身が圧縮後の`session.messages`と一致すること。
8. `/compact`コマンド: 閾値未満でも即座に圧縮が走ること、圧縮対象が無いときの案内メッセージが出ること。

## 受け入れ基準

1. `session.messages`の実測トークン数が`COMPACTION_THRESHOLD`を超えたとき、次のプロバイダ呼び出し前に自動で圧縮が走る。
2. 圧縮は直近`KEEP_RECENT_USER_TURNS`件のユーザーターンを完全な形で残し、それより前を1件の要約Messageに置き換える。ツール呼び出し/結果の対を割らない。
3. `/compact`で手動でも同じ圧縮を即座に呼び出せる。
4. 圧縮が起きたことがTUIの会話履歴に1行表示される。
5. TUIでの圧縮後、`/resume`で読み込んだセッションが圧縮後の状態(圧縮前の全履歴ではない)になる。
6. `polaris-cli`(exec)でも同じ圧縮機構が働く(永続化以外は差が無い)。
7. `cargo test --workspace`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo fmt --all -- --check`が通る。

## 見送った代替案

- **モデルごとのコンテキストウィンドウサイズを動的に取得する案**: より正確だが、polarisのどのプロバイダもその情報を返さず、新規に発見・保守する仕組みが要る。ブレインストーミングで固定閾値を選択、見送った。
- **要約せず直近N件だけ残して古いものを単純に切り捨てる案**: 実装は大幅に単純になるが、文脈を完全に失う。「codexに対して明確に優位」という目的に対し応答品質を落とす選択は取らない、ブレインストーミングで見送った。
- **個々の巨大メッセージを縮める仕組み(upstreamの`trim_function_call_history_to_fit_context_window`相当)**: upstreamでも圧縮とは別立ての独立した仕組みになっている規模で、今回のscopeでは見送り、既知の限界として残す。
- **サーバー側での要約(upstreamのremote compaction相当)**: polarisの各プロバイダにその機能が無く、実装コストに見合わない。ローカルでの1回のLLM呼び出しのみとした。
- **圧縮直後のキャッシュコールドへの特別な対応**: upstreamも同様に何もしていない(見つかった唯一の関連コメントは、圧縮リトライ時に「先頭からではなく末尾から削って直近を残す」という別の話)。今回のA/B実測(`docs/superpowers/CURRENT.md`参照)で3ターン目以降には93〜97%まで戻ることを確認済みのため、受け入れる。
