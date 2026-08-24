# polaris TUI ライブツール進捗・diff表示 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ツール呼び出し(read/write/edit/bash/skill)と`spawn`サブエージェントの活動を、ターン完了を待たずにリアルタイムでTUIへ表示し、`write`/`edit`は実際のdiff(コンテキスト行付き)を表示する。

**Architecture:** `polaris-core`に`Option<mpsc::UnboundedSender<AgentEvent>>`を`run`/`run_loop`/`dispatch`/`spawn::run_wave`/`run_one`へ追加引数として通す。CLI(headless)は`None`を渡すだけで無関係のまま。TUIはターン中の`tokio::select!`にイベント受信アームを追加し、受け取り次第`insert_before`で即座に印字する。`write`/`edit`は実行前後の内容から`similar`クレートで実diffを計算する。

**Tech Stack:** Rust、既存の`polaris-core`/`polaris-tui`クレート、tokio、新規依存として`similar`(diffクレート)。

**Spec:** `docs/superpowers/specs/2026-08-24-polaris-tui-live-tool-progress-design.md`

## Global Constraints

- 検証は毎タスク・コミット前に必ず `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` を実行する。
- `events: Option<...>` は既存の呼び出し全て(`polaris-cli`の一発実行、既存テスト)に`None`を渡すだけで無修正で動く形を維持する——`events`を`Option`ではなく必須引数にしない。
- サブエージェント(`spawn`)内部の`run_loop`呼び出しには`events: None`を渡す(深さ1の原則——サブエージェント内部の詳細はモデルにもTUIにも見せない、M4の既存設計と一貫させる)。
- ライブイベント表示の追加に伴い、`crates/polaris-tui/src/render.rs`の`history_lines_for`は`Role::User`/`Role::Assistant`の本文テキストのみを対象とし、`tool_calls`と`Role::Tool`メッセージはもう出力しない(ライブイベント側で既に表示済みのため——二重表示を避ける、仕様書に追記済みの決定)。

---

### Task 1: `AgentEvent`/`Diff` 型と `similar` 依存の追加

**Files:**
- Create: `crates/polaris-core/src/events.rs`
- Modify: `crates/polaris-core/src/lib.rs`(`mod events; pub use events::{AgentEvent, Diff, DiffHunk, DiffLine};` を追加)
- Modify: `Cargo.toml`(workspace、`[workspace.dependencies]` へ `similar = "2"` を追加)
- Modify: `crates/polaris-core/Cargo.toml`(`similar = { workspace = true }` を追加)

**Interfaces:**
- Consumes: なし(純粋なデータ型定義)
- Produces:
  - `pub enum AgentEvent { ToolStarted { name: String, detail: String }, ToolFinished { name: String, detail: String, ok: bool, diff: Option<Diff> }, SpawnStarted { agent_type: String, task: String }, SpawnFinished { agent_type: String, ok: bool } }`
  - `pub struct Diff { pub is_new_file: bool, pub hunks: Vec<DiffHunk>, pub added: usize, pub removed: usize }`
  - `pub struct DiffHunk { pub lines: Vec<DiffLine> }`
  - `pub enum DiffLine { Context(String), Added(String), Removed(String) }`
  - `pub fn compute_diff(old: &str, new: &str) -> Diff`(新規ファイル判定は呼び出し側が`is_new_file`を後から上書きする——`compute_diff`自体は「旧内容が空文字列」を「全行削除ゼロ・全行追加」として扱うだけで、ファイルの存在有無は知らない)

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/events.rs` を新規作成する。まずテストから書く。`similar`クレートの実際のAPI名(`TextDiff::from_lines`・`grouped_ops`・`ChangeTag`等)は、以下のコードが前提とする形と食い違えばコンパイルエラーとしてすぐ判明する——実装時に `cargo doc -p similar --open` で正確なメソッド名を確認し、同じ意味(2つのテキストを行単位で比較し、コンテキスト行付きのハンク列を得る)を持つ実際の呼び出しへ置き換えること。

```rust
//! ターン実行中にツール呼び出し・subagent活動をTUIへリアルタイム通知する
//! ためのイベント型。`polaris-cli`(一発実行)はこの型を一切知らずに
//! 動く——`agent::run`等への`events`引数は`Option`であり、`None`を渡す
//! だけで完全に無関係でいられる。

#[derive(Debug, Clone)]
pub enum AgentEvent {
    ToolStarted {
        name: String,
        detail: String,
    },
    ToolFinished {
        name: String,
        detail: String,
        ok: bool,
        diff: Option<Diff>,
    },
    SpawnStarted {
        agent_type: String,
        task: String,
    },
    SpawnFinished {
        agent_type: String,
        ok: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub is_new_file: bool,
    pub hunks: Vec<DiffHunk>,
    pub added: usize,
    pub removed: usize,
}

/// `old`/`new`を行単位で比較し、前後3行のコンテキスト付きハンク列を返す。
/// `is_new_file`は常に`false`で返す——ファイルが実際に新規かどうかは
/// 呼び出し側(`dispatch`、Task 3)が知っている情報であり、ここでは
/// 純粋に2つの文字列を比較するだけに留める。
pub fn compute_diff(old: &str, new: &str) -> Diff {
    use similar::{ChangeTag, TextDiff};

    let text_diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    for group in text_diff.grouped_ops(3) {
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let text = change.value().trim_end_matches('\n').to_string();
                match change.tag() {
                    ChangeTag::Equal => lines.push(DiffLine::Context(text)),
                    ChangeTag::Insert => {
                        added += 1;
                        lines.push(DiffLine::Added(text));
                    }
                    ChangeTag::Delete => {
                        removed += 1;
                        lines.push(DiffLine::Removed(text));
                    }
                }
            }
        }
        hunks.push(DiffHunk { lines });
    }

    Diff {
        is_new_file: false,
        hunks,
        added,
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_produces_no_hunks() {
        let diff = compute_diff("a\nb\nc\n", "a\nb\nc\n");
        assert!(diff.hunks.is_empty());
        assert_eq!(diff.added, 0);
        assert_eq!(diff.removed, 0);
    }

    #[test]
    fn a_single_line_change_is_reported_as_one_removed_and_one_added() {
        let diff = compute_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        assert!(!diff.hunks.is_empty());
    }

    #[test]
    fn appending_to_empty_old_content_counts_every_line_as_added() {
        let diff = compute_diff("", "a\nb\nc\n");
        assert_eq!(diff.added, 3);
        assert_eq!(diff.removed, 0);
    }

    #[test]
    fn context_lines_surround_a_change_up_to_three_lines_each_side() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\n3\n4\nCHANGED\n6\n7\n8\n9\n10\n";
        let diff = compute_diff(old, new);
        let context_count = diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| matches!(l, DiffLine::Context(_)))
            .count();
        // 変更行の前後3行ずつ、合計6行のコンテキストが含まれるはず
        // (前3行: 2,3,4 / 後3行: 6,7,8 — similarの実際のグルーピング
        // 挙動次第で若干前後する可能性があるため、0件でないことと
        // 6行を超えないことだけを固定する)。
        assert!(context_count > 0);
        assert!(context_count <= 6);
    }
}
```

Run: `cargo test -p polaris-core events::`

Expected: FAIL — `similar`が依存に無いためコンパイルエラー。

- [ ] **Step 2: `similar`依存を追加する**

workspace の `Cargo.toml` の `[workspace.dependencies]` へ追加する。

```toml
similar = "2"
```

`crates/polaris-core/Cargo.toml` の `[dependencies]` へ追加する。

```toml
similar = { workspace = true }
```

- [ ] **Step 3: テストを通す**

Run: `cargo test -p polaris-core events::`

Expected: PASS(4 tests)。`similar`の実際のAPIがこのステップのコード例と食い違う場合(メソッド名・型名の違い)、`cargo doc -p similar --open`で正確な名前を確認し、同じ意味を持つ実際の呼び出しへ書き換えてからテストを通すこと。

- [ ] **Step 4: `lib.rs`へ配線する**

```rust
mod events;
pub use events::{AgentEvent, Diff, DiffHunk, DiffLine, compute_diff};
```

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add Cargo.toml crates/polaris-core/Cargo.toml crates/polaris-core/src/events.rs crates/polaris-core/src/lib.rs
git commit -m "feat(polaris-core): add AgentEvent/Diff types for live tool progress

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: `run`/`run_loop`/`dispatch` へ `events` を通す(read/bash/skill)

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`
- Modify: `crates/polaris-core/src/spawn.rs`(`run_one`/`run_wave`が`dispatch`を経由しないため、この時点では`events: None`を`run_loop`呼び出しへ渡すだけの機械的な追従)
- Modify: `crates/polaris-cli/src/main.rs`(`agent::run`呼び出しへ`events: None`を追加)
- Modify: `crates/polaris-tui/src/lib.rs`(同様に`events: None`を一時的に渡す——実際にチャンネルを使うのはTask 5)

**Interfaces:**
- Consumes: Task 1 の `AgentEvent`
- Produces: `run`/`run_loop`/`dispatch`の新シグネチャ(末尾から2番目、`ctx`の直前に`events: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>`を追加)

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs`の既存テストモジュールに追加する。既存の`run`呼び出しパターン(`spawn.rs`のテストや`agent.rs`自身の既存テストが使っている形)に倣い、`events`にチャンネルの送信側を渡して`read`ツール呼び出し1回で`ToolStarted`→`ToolFinished`の順に届くことを確認する。

```rust
    #[tokio::test]
    async fn read_dispatches_tool_started_and_finished_events_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let provider = Scripted::new(vec![
            scripted_tool_call("read", serde_json::json!({"path": dir.path().join("a.txt")})),
            scripted_final_text("done"),
        ]);
        let mut session = Session::new();
        session.push_user("read a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::ReadOnly,
            &[],
        )
        .unwrap();
        let helper = std::path::PathBuf::from("/bin/true");
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &provider,
            &mut session,
            audit.clone(),
            &mut stop,
            &crate::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            std::sync::Arc::new(provider.clone()),
            8,
            4,
            Some(tx),
            &mut ctx,
        )
        .await
        .unwrap();

        let first = rx.recv().await.unwrap();
        assert!(matches!(first, AgentEvent::ToolStarted { ref name, .. } if name == "read"));
        let second = rx.recv().await.unwrap();
        assert!(matches!(second, AgentEvent::ToolFinished { ref name, ok: true, .. } if name == "read"));
    }
```

`Scripted`が`Clone`を実装していない場合の対処、`scripted_tool_call`/`scripted_final_text`/`shared_audit`/`AlwaysAllow`の正確な既存ヘルパ名は、`agent.rs`の既存テストモジュールを直接確認して合わせること(Task 9の先行実装で確立済みのパターンがあるはず)。

Run: `cargo test -p polaris-core read_dispatches_tool_started_and_finished_events_in_order`

Expected: FAIL — `events`引数が未定義でコンパイルエラー。

- [ ] **Step 2: `run`/`run_loop`/`dispatch`のシグネチャへ`events`を追加する**

`crates/polaris-core/src/agent.rs`の3関数それぞれへ、`ctx: &mut ToolContext<'_>`の直前に引数を追加する。

```rust
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
```

`run`は受け取った`events`を`run_loop`へそのまま渡す。`run_loop`は`dispatch`呼び出しへ`events.clone()`を渡す(`UnboundedSender`は`Clone`——1ターンに複数回のツール呼び出しがあるため、`dispatch`呼び出しのたびに複製する)。

`dispatch`の本体、`match call.name.as_str()`の直前に、共通の「開始イベント送信」を追加する。

```rust
    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
        });
    }

    let outcome: Result<String, String> = match call.name.as_str() {
        // 既存の "read"/"write"/"edit"/"bash"/"skill"/"spawn" の6腕、本体は
        // このタスクでは無修正(read/bash/skillのみ)。
        ...
    };

    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolFinished {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
            ok: outcome.is_ok(),
            diff: None, // write/editのdiffはTask 3で埋める
        });
    }

    outcome
```

既存の`match`の戻り値を直接returnしていた形から、`outcome`という束縛を経由してから最後に返す形へ変える(`?`を使っている既存の各腕はそのまま——`match`全体の型が`Result<String, String>`である点は変わらない)。

- [ ] **Step 3: 呼び出し元を機械的に追従させる**

`cargo build --workspace --tests 2>&1 | head -100` を実行し、コンパイルエラーが指す箇所を1つずつ直す。具体的には:
- `crates/polaris-core/src/spawn.rs`の`run_one`内、`run_loop`呼び出し2箇所(初回・リトライ)へ`events: None`を追加(深さ1の原則——サブエージェント内部はイベント経路に乗らない、Global Constraints参照)。
- `crates/polaris-core/src/spawn.rs`の`dispatch`呼び出し(もしあれば、無ければスキップ)。
- `crates/polaris-cli/src/main.rs`の`agent::run`呼び出しへ`events: None`を追加。
- `crates/polaris-tui/src/lib.rs`の`agent::run`呼び出し2箇所(通常ターン・テスト用ヘルパ)へ、いったん`events: None`を追加(Task 5で実際のチャンネルに置き換える)。
- 既存テスト内の`run`/`run_loop`/`dispatch`への直接呼び出し全てへ`events: None`(または`events`を検証したいテストのみ実際のチャンネル)を追加。

コンパイラの型エラーが機械的に指し示すため、見落としは起きない。

- [ ] **Step 4: テストを通す**

Run: `cargo test -p polaris-core read_dispatches_tool_started_and_finished_events_in_order`

Expected: PASS。

- [ ] **Step 5: 既存テストが全て無修正の挙動のまま通ることを確認する**

Run: `cargo test --workspace`

Expected: 全テストが通る——`events: None`のケースは今まで通りの挙動(イベント送信の分岐が単に何もしないだけ)。

- [ ] **Step 6: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/agent.rs crates/polaris-core/src/spawn.rs crates/polaris-cli/src/main.rs crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-core): thread optional AgentEvent notifications through dispatch

read/bash/skillの開始・終了を通知する。write/editのdiffはTask 3、
spawnの通知はTask 4で追加する。polaris-cli(一発実行)はNoneを渡す
だけで無関係のまま、既存挙動は一切変えていない。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: `write`/`edit` の diff 計算とイベント送信

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`

**Interfaces:**
- Consumes: Task 1 の `compute_diff`、Task 2 で追加済みの `events` 引数
- Produces: `dispatch`の`"write"`/`"edit"`腕が、実際のdiffを`ToolFinished.diff`へ詰めて送信する

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs`の既存テストモジュールに追加する。

```rust
    #[tokio::test]
    async fn writing_a_new_file_reports_a_diff_with_only_added_lines() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new.txt");
        let provider = Scripted::new(vec![
            scripted_tool_call(
                "write",
                serde_json::json!({"path": target, "content": "line1\nline2\n"}),
            ),
            scripted_final_text("done"),
        ]);
        let mut session = Session::new();
        session.push_user("create new.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[dir.path().to_path_buf()],
        )
        .unwrap();
        let helper = std::path::PathBuf::from("/bin/true");
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &provider,
            &mut session,
            audit.clone(),
            &mut stop,
            &crate::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            std::sync::Arc::new(provider.clone()),
            8,
            4,
            Some(tx),
            &mut ctx,
        )
        .await
        .unwrap();

        let _started = rx.recv().await.unwrap();
        let finished = rx.recv().await.unwrap();
        let AgentEvent::ToolFinished { diff, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        let diff = diff.expect("write should report a diff");
        assert!(diff.is_new_file);
        assert_eq!(diff.added, 2);
        assert_eq!(diff.removed, 0);
    }

    #[tokio::test]
    async fn writing_over_an_existing_file_reports_a_diff_against_its_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("existing.txt");
        std::fs::write(&target, "old line\n").unwrap();
        let provider = Scripted::new(vec![
            scripted_tool_call(
                "write",
                serde_json::json!({"path": target, "content": "new line\n"}),
            ),
            scripted_final_text("done"),
        ]);
        let mut session = Session::new();
        session.push_user("overwrite existing.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[dir.path().to_path_buf()],
        )
        .unwrap();
        let helper = std::path::PathBuf::from("/bin/true");
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &provider,
            &mut session,
            audit.clone(),
            &mut stop,
            &crate::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            std::sync::Arc::new(provider.clone()),
            8,
            4,
            Some(tx),
            &mut ctx,
        )
        .await
        .unwrap();

        let _started = rx.recv().await.unwrap();
        let finished = rx.recv().await.unwrap();
        let AgentEvent::ToolFinished { diff, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        let diff = diff.expect("overwriting should report a diff");
        assert!(!diff.is_new_file);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }
```

Run: `cargo test -p polaris-core writing_a_new_file_reports_a_diff writing_over_an_existing_file_reports_a_diff`

Expected: FAIL — `diff`は常に`None`のまま。

- [ ] **Step 2: `"write"`腕を実装する**

`crates/polaris-core/src/agent.rs`の`dispatch`内、`"write"`腕を次のように変える。

```rust
        "write" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path is missing".to_string())?;
            let content = call.arguments["content"]
                .as_str()
                .ok_or_else(|| "content is missing".to_string())?;
            let path = Path::new(path);
            ctx.gate.check(ctx.sandbox, path, ctx.approver)?;
            // diffを送るため、上書き前の内容を先に読んでおく。読めない
            // (=存在しない)なら新規ファイル扱い。読み取り自体の失敗は
            // write本体の成否に影響させない——diff計算はベストエフォート
            // の副作用であり、readできないことがwrite自体を失敗させる
            //理由にはならない。
            let old_content = std::fs::read_to_string(path).ok();
            let result = polaris_tools::write::write(ctx.sandbox, ctx.helper, path, content)
                .map_err(|e| e.to_string());
            if result.is_ok() {
                let mut diff = crate::events::compute_diff(
                    old_content.as_deref().unwrap_or(""),
                    content,
                );
                diff.is_new_file = old_content.is_none();
                pending_diff = Some(diff);
            }
            result
        }
```

`"edit"`腕も同様に変える。

```rust
        "edit" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path is missing".to_string())?;
            let old = call.arguments["old"]
                .as_str()
                .ok_or_else(|| "old is missing".to_string())?;
            let new = call.arguments["new"]
                .as_str()
                .ok_or_else(|| "new is missing".to_string())?;
            let path = Path::new(path);
            ctx.gate.check(ctx.sandbox, path, ctx.approver)?;
            let result = polaris_tools::edit::edit(ctx.sandbox, ctx.helper, path, old, new)
                .map_err(|e| e.to_string());
            if result.is_ok() {
                pending_diff = Some(crate::events::compute_diff(old, new));
            }
            result
        }
```

`pending_diff`は、Step 2で追加した`match`ブロックの直前に`let mut pending_diff: Option<crate::events::Diff> = None;`として宣言し、`match`の後、`ToolFinished`送信時に使う。

```rust
    let mut pending_diff: Option<crate::events::Diff> = None;
    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
        });
    }

    let outcome: Result<String, String> = match call.name.as_str() {
        // ... 全6腕、write/editは上記の通り pending_diff を埋める
    };

    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolFinished {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
            ok: outcome.is_ok(),
            diff: pending_diff,
        });
    }

    outcome
```

- [ ] **Step 3: テストを通す**

Run: `cargo test -p polaris-core writing_a_new_file_reports_a_diff writing_over_an_existing_file_reports_a_diff`

Expected: PASS。

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/agent.rs
git commit -m "feat(polaris-core): compute and report a real diff for write/edit events

writeは実行前に上書き対象の既存内容を読み(存在しなければ新規ファイル
扱い)、editは引数のold/newをそのまま使う。diff計算の失敗・旧内容の
読み取り失敗はwrite/edit本体の成否に影響しない——ベストエフォートの
副作用として扱う。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: `spawn` のイベント通知(`SpawnStarted`/`SpawnFinished`)

**Files:**
- Modify: `crates/polaris-core/src/spawn.rs`
- Modify: `crates/polaris-core/src/agent.rs`(`dispatch`の`"spawn"`腕、`run_wave`呼び出しへ`events`を追加)

**Interfaces:**
- Consumes: Task 1 の `AgentEvent::{SpawnStarted, SpawnFinished}`、Task 2 で `dispatch` に追加済みの `events`
- Produces: `run_wave`/`run_one`の新シグネチャ(末尾に`events: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>`を追加)

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/spawn.rs`の既存テストモジュールに追加する。既存の`readonly_fixture_agent_type`等のフィクスチャヘルパをそのまま使う。

```rust
    #[tokio::test]
    async fn run_one_reports_spawn_started_and_finished() {
        let agent_types = vec![readonly_fixture_agent_type()];
        let provider = counting_mock_provider_returning_valid_output(
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let audit = shared_test_audit();
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::ReadOnly,
            &[],
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = SpawnTask {
            agent_type: "ro-fixture".to_string(),
            task: "do something".to_string(),
            write_root: None,
        };

        let _outcome = run_one(
            &task,
            &agent_types,
            std::sync::Arc::new(provider),
            audit,
            &sandbox,
            std::path::Path::new("/bin/true"),
            Some(tx),
        )
        .await;

        let started = rx.recv().await.unwrap();
        assert!(matches!(
            started,
            AgentEvent::SpawnStarted { ref agent_type, .. } if agent_type == "ro-fixture"
        ));
        let finished = rx.recv().await.unwrap();
        assert!(matches!(finished, AgentEvent::SpawnFinished { .. }));
    }

    #[tokio::test]
    async fn a_subagents_own_tool_calls_never_reach_the_events_channel() {
        // 深さ1の原則: サブエージェント自身のrun_loop呼び出しには
        // events: None が渡る(Task 2で確立済み)。run_one自体が送るのは
        // SpawnStarted/SpawnFinishedの2件だけであることを確認する。
        let agent_types = vec![readonly_fixture_agent_type()];
        let provider = counting_mock_provider_returning_valid_output(
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let audit = shared_test_audit();
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::ReadOnly,
            &[],
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = SpawnTask {
            agent_type: "ro-fixture".to_string(),
            task: "do something".to_string(),
            write_root: None,
        };

        let _outcome = run_one(
            &task,
            &agent_types,
            std::sync::Arc::new(provider),
            audit,
            &sandbox,
            std::path::Path::new("/bin/true"),
            Some(tx),
        )
        .await;

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 0, "expected exactly 2 events already drained above, none left");
    }
```

Run: `cargo test -p polaris-core run_one_reports_spawn_started_and_finished a_subagents_own_tool_calls_never_reach_the_events_channel`

Expected: FAIL — `run_one`に`events`引数が無くコンパイルエラー。

- [ ] **Step 2: `run_one`/`run_wave`のシグネチャへ`events`を追加する**

`crates/polaris-core/src/spawn.rs`の`run_one`・`run_wave`双方の末尾引数として追加する。

```rust
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
```

`run_one`内、実際のsubagent実行(`run_loop`呼び出し)の前後で送信する。

```rust
    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::SpawnStarted {
            agent_type: task.agent_type.clone(),
            task: task.task.clone(),
        });
    }

    // 既存の run_loop 呼び出し(初回・リトライとも events: None のまま
    // ——深さ1の原則、Task 2で確立済み)

    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::SpawnFinished {
            agent_type: task.agent_type.clone(),
            ok: matches!(outcome, TaskOutcome::Ok(_)),
        });
    }
```

`run_wave`は受け取った`events`を、並列実行する各`run_one`呼び出しへ`.clone()`して渡す(`UnboundedSender`は`Clone`)。

- [ ] **Step 3: `dispatch`の`"spawn"`腕を追従させる**

`crates/polaris-core/src/agent.rs`の`"spawn"`腕、`crate::spawn::run_wave(...)`呼び出しへ`events.clone()`を追加する。

- [ ] **Step 4: テストを通す**

Run: `cargo test -p polaris-core run_one_reports_spawn_started_and_finished a_subagents_own_tool_calls_never_reach_the_events_channel`

Expected: PASS。

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/spawn.rs crates/polaris-core/src/agent.rs
git commit -m "feat(polaris-core): report SpawnStarted/SpawnFinished for spawn tasks

サブエージェント自身のrun_loop呼び出しにはevents: Noneを渡す——
深さ1の原則により、サブエージェント内部の詳細はこのイベント経路にも
乗らない。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: TUI — イベント受信とライブ印字(通常ツール・spawn)

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`
- Modify: `crates/polaris-tui/src/render.rs`(`history_lines_for`から`tool_calls`/`Role::Tool`の出力を除去——Global Constraints参照)

**Interfaces:**
- Consumes: Task 1-4 の `AgentEvent`
- Produces: `crates/polaris-tui/src/render.rs`に `pub fn format_event_for_live_print(event: &AgentEvent) -> Vec<HistoryLine>`(diffの整形はTask 6で拡張、このタスクでは`Tool`/`Spawn`イベントのみ)

- [ ] **Step 1: `history_lines_for`から二重表示分を除去する**

`crates/polaris-tui/src/render.rs`の`history_lines_for`(247行目付近)から、`m.tool_calls`を処理していたブロックと`Role::Tool`の腕を削除し、`Role::User | Role::Assistant`の本文テキストのみを扱う形にする。既存テストのうち、`tool_calls`や`Role::Tool`の出力を前提にしていたものは、この変更に合わせてアサーションを更新する(該当テストを`grep -n "tool_calls\|Role::Tool" crates/polaris-tui/src/render.rs`で洗い出し、1件ずつ確認すること)。

Run: `cargo test -p polaris-tui render::`

Expected: 洗い出したテストの分だけ最初はFAILする。本文テキストのみを検証する形に書き換えてPASSさせる。

- [ ] **Step 2: `format_event_for_live_print`を実装する**

`crates/polaris-tui/src/render.rs`へ追加する。

```rust
/// `AgentEvent`をターン実行中にそのまま印字できる`HistoryLine`列へ変換
/// する。diffを持つ`ToolFinished`(write/edit)の整形はTask 6で拡張する
/// ——このタスクでは`diff`フィールドを無視し、通常のツール・spawnの
/// 開始/終了表示だけを扱う。
pub fn format_event_for_live_print(event: &AgentEvent) -> Vec<HistoryLine> {
    match event {
        AgentEvent::ToolStarted { name, detail } => {
            let preview: String = detail.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            vec![HistoryLine::plain(Line::from(Span::raw(sanitize(&format!(
                "⏺ {name}({preview})"
            )))))]
        }
        AgentEvent::ToolFinished { ok, .. } => {
            let marker = if *ok { "done" } else { "failed" };
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!("  {marker}")),
                Style::default().add_modifier(Modifier::DIM),
            )))]
        }
        AgentEvent::SpawnStarted { agent_type, task } => {
            let preview: String = task.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            vec![
                HistoryLine::plain(Line::from(Span::raw(sanitize(&format!(
                    "⏺ Agent({agent_type}: {preview})"
                ))))),
                HistoryLine::plain(Line::from(Span::styled(
                    "  Backgrounded agent",
                    Style::default().add_modifier(Modifier::DIM),
                ))),
            ]
        }
        AgentEvent::SpawnFinished { agent_type, ok } => {
            let marker = if *ok { "done" } else { "failed" };
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!("  {agent_type}: {marker}")),
                Style::default().add_modifier(Modifier::DIM),
            )))]
        }
    }
}
```

`AgentEvent`を`render.rs`から使うため、`use polaris_core::events::AgentEvent;`を追加する。

- [ ] **Step 2a: テストを書く**

```rust
    #[test]
    fn a_tool_started_event_renders_as_a_bullet_line() {
        let event = AgentEvent::ToolStarted {
            name: "bash".to_string(),
            detail: "{\"command\":\"ls\"}".to_string(),
        };
        let lines = format_event_for_live_print(&event);
        let text: String = lines[0]
            .line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("⏺"));
        assert!(text.contains("bash"));
    }

    #[test]
    fn a_spawn_started_event_mentions_backgrounded_agent() {
        let event = AgentEvent::SpawnStarted {
            agent_type: "file-inspector".to_string(),
            task: "inspect agent.rs".to_string(),
        };
        let lines = format_event_for_live_print(&event);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.line.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("Agent"));
        assert!(joined.contains("Backgrounded agent"));
    }
```

Run: `cargo test -p polaris-tui format_event_for_live_print a_tool_started a_spawn_started`

Expected: PASS。

- [ ] **Step 3: `lib.rs`のターンループへチャンネルを配線する**

`crates/polaris-tui/src/lib.rs`のターン実行部、既存の`tokio::pin!(agent_future)`の直前でチャンネルを作る。

```rust
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
```

`agent::run(...)`呼び出しの`events`引数(Task 2で`None`を渡していた箇所)を`Some(events_tx)`へ置き換える。

既存の`tokio::select! { biased; ... }`へ、5つ目のアームとして追加する(`agent_future`より後、`ticker.tick()`より前——`biased`セレクトなので記述順が優先度そのもの。ターン完了自体を最優先、次にイベント、最後にステータス更新tick、という順にする)。

```rust
                tokio::select! {
                    biased;
                    result = &mut agent_future => break TurnOutcome::Done(result),
                    Some(event) = events_rx.recv() => {
                        let lines = render::format_event_for_live_print(&event);
                        if terminal
                            .borrow_mut()
                            .insert_before(lines.len() as u16, |buf| {
                                render::render_history_into(buf, buf.area, &lines)
                            })
                            .is_err()
                        {
                            break TurnOutcome::Fatal;
                        }
                    }
                    _ = ticker.tick() => {
                        // 既存のまま
                    }
                    maybe_event = event_stream.next() => {
                        // 既存のまま
                    }
                }
```

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs crates/polaris-tui/src/render.rs
git commit -m "feat(polaris-tui): print tool/spawn activity live during a turn

history_lines_forはtool_calls/Role::Toolの出力をやめた——ライブ表示
側で既に印字済みのため、ターン完了後の一括印字と二重表示にならない
ようにした。diffの整形(write/edit)はTask 6で追加する。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: diff のライブ表示・切り詰め

**Files:**
- Modify: `crates/polaris-tui/src/render.rs`

**Interfaces:**
- Consumes: Task 1 の `Diff`/`DiffHunk`/`DiffLine`
- Produces: `format_event_for_live_print`の`ToolFinished { diff: Some(_), .. }`分岐の拡張

- [ ] **Step 1: 失敗するテストを書く**

```rust
    #[test]
    fn a_tool_finished_event_with_a_diff_renders_added_and_removed_lines_with_a_header() {
        let diff = polaris_core::events::Diff {
            is_new_file: false,
            hunks: vec![polaris_core::events::DiffHunk {
                lines: vec![
                    polaris_core::events::DiffLine::Context("unchanged".to_string()),
                    polaris_core::events::DiffLine::Removed("old line".to_string()),
                    polaris_core::events::DiffLine::Added("new line".to_string()),
                ],
            }],
            added: 1,
            removed: 1,
        };
        let event = AgentEvent::ToolFinished {
            name: "write".to_string(),
            detail: "{\"path\":\"a.txt\"}".to_string(),
            ok: true,
            diff: Some(diff),
        };
        let lines = format_event_for_live_print(&event);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.line.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Added 1 lines, removed 1 lines") || joined.contains("+1") && joined.contains("-1"));
        assert!(joined.contains("old line"));
        assert!(joined.contains("new line"));
    }

    #[test]
    fn a_diff_longer_than_the_cap_is_truncated_with_a_notice() {
        let many_added: Vec<polaris_core::events::DiffLine> = (0..100)
            .map(|i| polaris_core::events::DiffLine::Added(format!("line {i}")))
            .collect();
        let diff = polaris_core::events::Diff {
            is_new_file: true,
            hunks: vec![polaris_core::events::DiffHunk { lines: many_added }],
            added: 100,
            removed: 0,
        };
        let event = AgentEvent::ToolFinished {
            name: "write".to_string(),
            detail: "{\"path\":\"big.txt\"}".to_string(),
            ok: true,
            diff: Some(diff),
        };
        let lines = format_event_for_live_print(&event);
        // ヘッダー行+切り詰め通知1行を除いても、40行を大きく超えない
        // ことを固定する(既存のTOOL_RESULT_PREVIEW_CHARSと同じ思想の
        // 上限——正確な行数は実装時に決める定数を参照する)。
        assert!(lines.len() < 100);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.line.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("省略") || joined.to_lowercase().contains("omitted") || joined.contains("more"));
    }
```

Run: `cargo test -p polaris-tui a_tool_finished_event_with_a_diff a_diff_longer_than_the_cap`

Expected: FAIL — `diff`は現状無視されている。

- [ ] **Step 2: 実装する**

`crates/polaris-tui/src/render.rs`の`format_event_for_live_print`内、`AgentEvent::ToolFinished`腕を拡張する。

```rust
const MAX_DIFF_LINES_SHOWN: usize = 40;

        AgentEvent::ToolFinished { name, ok, diff, .. } => {
            let mut lines = vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!("  {}", if *ok { "done" } else { "failed" })),
                Style::default().add_modifier(Modifier::DIM),
            )))];
            if let Some(d) = diff {
                let kind = if d.is_new_file { "Created" } else { "Updated" };
                lines.push(HistoryLine::plain(Line::from(Span::styled(
                    sanitize(&format!(
                        "  {kind} — Added {} lines, removed {} lines",
                        d.added, d.removed
                    )),
                    Style::default().add_modifier(Modifier::DIM),
                ))));
                let mut shown = 0usize;
                'hunks: for hunk in &d.hunks {
                    for dl in &hunk.lines {
                        if shown >= MAX_DIFF_LINES_SHOWN {
                            lines.push(HistoryLine::plain(Line::from(Span::styled(
                                "  ...(省略)",
                                Style::default().add_modifier(Modifier::DIM),
                            ))));
                            break 'hunks;
                        }
                        let (prefix, style) = match dl {
                            DiffLine::Context(_) => ("  ", Style::default().add_modifier(Modifier::DIM)),
                            DiffLine::Added(_) => ("+ ", Style::default().fg(Color::Green)),
                            DiffLine::Removed(_) => ("- ", Style::default().fg(Color::Red)),
                        };
                        let text = match dl {
                            DiffLine::Context(s) | DiffLine::Added(s) | DiffLine::Removed(s) => s,
                        };
                        lines.push(HistoryLine::plain(Line::from(Span::styled(
                            sanitize(&format!("{prefix}{text}")),
                            style,
                        ))));
                        shown += 1;
                    }
                }
            }
            let _ = name;
            lines
        }
```

- [ ] **Step 3: テストを通す**

Run: `cargo test -p polaris-tui a_tool_finished_event_with_a_diff a_diff_longer_than_the_cap`

Expected: PASS。テストの`"Added 1 lines, removed 1 lines"`という文言と実装の文言が完全一致しない場合、テストのアサーションを実装の実際の文言に合わせて調整すること(文言そのものはこのステップで決めてよい——受け入れ基準は「追加/削除件数がわかること」であり、一言一句の完全一致ではない)。

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/render.rs
git commit -m "feat(polaris-tui): render write/edit diffs live, truncated at 40 lines

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: 受け入れ基準の直接検証

**Files:**
- Modify: `crates/polaris-cli/tests/cli.rs`(または既存の統合テストファイル、`polaris-cli`が`events`引数追加の影響を一切受けないことの確認)
- Modify: `crates/polaris-core/src/spawn.rs`(深さ1のイベント非漏洩、Task 4で既に1本テスト済みだが、念のためTask 4の視点と異なる形でもう1本)

**Interfaces:** 新しい公開APIは無い。既存機能に対するテストのみ追加する。

- [ ] **Step 1: `polaris-cli`(一発実行)が無影響であることを確認する**

`crates/polaris-cli/tests/cli.rs`の既存テスト群を確認し、`events`引数追加後も無修正で通っていることを`cargo test -p polaris-cli`で確認する(既存テストが全てそのまま通ることそのものが受け入れ基準4の証明——新しいテストを書く必要は本来ないが、意図を明示するため、`agent::run`に`events: None`を渡す一発実行の統合テストが1本もない場合は次のテストを追加する)。

```rust
// crates/polaris-core/src/agent.rs の既存テストモジュールへ追加
    #[tokio::test]
    async fn a_run_call_with_no_events_channel_behaves_identically_to_before(
    ) {
        // events: None を渡す既存の全テスト(Task 2以前からあるもの)が
        // 無修正のまま通っていること自体がこの受け入れ基準の主要な証拠。
        // このテストは「Noneを渡すコードパス自体がパニックしない」こと
        // を明示的に固定する。
        let dir = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![scripted_final_text("done")]);
        let mut session = Session::new();
        session.push_user("hello");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::ReadOnly,
            &[],
        )
        .unwrap();
        let helper = std::path::PathBuf::from("/bin/true");
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let result = run(
            &provider,
            &mut session,
            audit.clone(),
            &mut stop,
            &crate::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            std::sync::Arc::new(provider.clone()),
            8,
            4,
            None,
            &mut ctx,
        )
        .await;

        assert!(result.is_ok());
    }
```

Run: `cargo test -p polaris-core a_run_call_with_no_events_channel_behaves_identically_to_before`

Expected: PASS。

- [ ] **Step 2: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/agent.rs
git commit -m "test: pin that events: None leaves run() behaving exactly as before

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 自己レビュー記録

- **spec 網羅性:** イベント型・通知経路(通常ツール) → Task 1/2。write/editのdiff → Task 3。spawn通知・深さ1維持 → Task 4。TUI側のライブ印字・二重表示の除去 → Task 5。diff表示・切り詰め → Task 6。CLI無影響の直接検証 → Task 7。
- **型の一貫性:** `AgentEvent`/`Diff`/`DiffHunk`/`DiffLine`(Task 1)はTask 2-6まで一貫して使われる。`events: Option<UnboundedSender<AgentEvent>>`という引数名・型は`run`/`run_loop`/`dispatch`/`run_wave`/`run_one`全てで統一する。
- **見つかったが計画に含めなかったもの:** `polaris-cli`(一発実行)側でのライブ表示自体(仕様書で明記済みのスコープ外)。diffの構文ハイライト(仕様書で明記済みのスコープ外)。
