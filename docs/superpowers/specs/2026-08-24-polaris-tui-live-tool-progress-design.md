# polaris TUI ライブツール進捗・diff表示 設計書

## 背景・目的

現在のpolaris TUI(インラインビューポート方式、直前の変更で導入済み)は、1ターン中の`read`/`write`/`edit`/`bash`/`skill`呼び出しや`spawn`サブエージェントの活動を一切表示せず、ターン完了後にまとめて印字する。これは`agent::run`がターン全体を通じて`session`を排他的に借用する設計(`&mut Session`)によるもので、TUI側は完了までその中身を覗けない。

本設計は、Claude Code自身のUI(`⏺ Update(path)` / `⏺ Agent(...)` 形式のライブ表示)に倣い、ツール呼び出し・サブエージェント実行の進捗をターン実行中にリアルタイムで表示し、`write`/`edit`についてはGitHubのdiffビュー相当(前後数行のコンテキスト付き)を表示する。

## スコープ

対象: `polaris-core`(イベント通知基盤)・`polaris-tui`(受信・表示)。`polaris-cli`(一発実行・headless)の既存挙動には一切影響しない(イベント送信は完全にオプション)。

非対象: `polaris-cli`側でのライブ表示(将来別途検討)。diffの構文ハイライト(言語別の色分け)は今回のスコープ外——追加行/削除行の色分けのみ。

## アーキテクチャ

```
[agent::dispatch] 各ツール呼び出しの前後
        │
        ├─ ToolStarted { name, detail } を送信
        │
   (ツール本体の実行、write/editは実行前に旧内容を読む)
        │
        ├─ ToolFinished { name, detail, result, diff: Option<Diff> } を送信
        │
[spawn::run_one] 各サブエージェントタスクの前後
        │
        ├─ SpawnStarted { agent_type, task } を送信
        │
   (subagentの run_loop 実行——内部のツール呼び出しは
    このイベント経路には乗らない。深さ1の原則どおり、
    subagent内部の詳細はモデルにもTUIにも見せない)
        │
        └─ SpawnFinished { agent_type, outcome } を送信
        │
[polaris-tui のターンループ]
tokio::select! の受信アームでイベントを受け取り次第
insert_before で即座に印字(⏺ Tool(...) / ⏺ Agent(...) / diff)
```

## コンポーネント

### 1. `AgentEvent`(`crates/polaris-core/src/events.rs`、新設)

```rust
pub enum AgentEvent {
    ToolStarted { name: String, detail: String },
    ToolFinished { name: String, detail: String, ok: bool, diff: Option<Diff> },
    SpawnStarted { agent_type: String, task: String },
    SpawnFinished { agent_type: String, ok: bool },
}

pub struct Diff {
    pub is_new_file: bool,
    /// 前後数行のコンテキスト付き、行の追加・削除を持つハンク列。
    pub hunks: Vec<DiffHunk>,
    pub added: usize,
    pub removed: usize,
}
```

具体的なハンク構造は`similar`クレート(新規依存、`unified_diff`/`grouped_ops`相当のAPI)の出力形式に合わせて実装時に確定する。

### 2. イベント送信経路(`crates/polaris-core/src/agent.rs`)

`run`/`run_loop`/`dispatch`のシグネチャへ`events: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>`を追加する(既存の`agent_types`/`provider_pool`と同様、`Arc`ではなく`Option`——送信先が無ければ`if let Some(tx) = &events { let _ = tx.send(...); }`で無視するだけ)。`polaris-cli`(一発実行)は`None`を渡すだけで済み、既存の呼び出し・既存のテストは無修正。

`dispatch`の各腕(read/write/edit/bash/skill)で、実行前に`ToolStarted`、実行後に`ToolFinished`を送る。`write`/`edit`のみ`diff`フィールドを埋める:
- `edit`: 引数の`old`/`new`文字列から直接diffを計算。
- `write`: 実行前に対象パスの既存内容を`std::fs::read_to_string`相当で読む(存在しなければ`is_new_file: true`で全行追加扱い)。

### 3. `spawn::run_one`(既存、M4)

同様に`events`を受け取り、`SpawnStarted`/`SpawnFinished`を送る。サブエージェント自身の`run_loop`呼び出しには`events: None`を渡す——深さ1の原則(サブエージェント内部の詳細はモデルにもTUIにも見せない)を、イベント通知でも一貫させる。

### 4. TUI受信・表示(`crates/polaris-tui/src/lib.rs`)

ターン開始時に`tokio::sync::mpsc::unbounded_channel()`を作り、送信側を`agent::run`へ渡す。既存の`tokio::select!`(ticker/agent_future/event_stream)へ4つ目のアームとして受信を追加し、イベントを受け取り次第`terminal.insert_before(...)`で即座に印字する——「ターン完了後にまとめて印字」ではなく「発生した瞬間に印字」という、既存の`print_new_history`とは別の、もう一つの一度きり印字経路になる。

表示形式(Claude Code自身の実際の表示に倣う):
- 通常ツール: `⏺ Tool(引数)` → 完了時 `→ 結果プレビュー`(既存の`format_tool_result`のプレビュー方式を流用)。
- spawn: `⏺ Agent(task内容)` → 完了時 `完了` または失敗表示。
- write/edit: `⏺ Update(path)` の下に `Added N lines, removed M lines` ヘッダー、続けてdiff本体(追加行は緑+`+`、削除行は赤+`-`、コンテキスト行は無地)。40行を超えたら`...以下N行省略`で切り詰める(既存の`TOOL_RESULT_PREVIEW_CHARS`と同じ思想)。

## テスト方針

1. `AgentEvent`送信が既存の`agent::run`/`dispatch`のテストに影響しないこと(`events: None`のケースが今まで通り動く)。
2. `dispatch`の各腕が正しいタイミング(実行前/実行後)で正しい`AgentEvent`を送ることを、モックの受信チャンネルで確認する統合テスト。
3. `write`/`edit`のdiff計算——新規ファイル・既存ファイル上書き・複数箇所の変更それぞれで、`added`/`removed`件数とハンク内容が正しいことを確認する単体テスト。
4. TUI側のイベント→即時印字ロジックを、実際のsubagent実行を伴わずに(モックチャンネルへ直接イベントを送り込む形で)確認するテスト。
5. `spawn`のイベントが深さ1を超えて漏れない(サブエージェント内部のツール呼び出しがこの経路に乗らない)ことを確認するテスト。

## 受け入れ基準

1. 通常ツール呼び出しが、ターン完了を待たずに`⏺ Tool(...)`として即座に表示される。
2. `spawn`呼び出しが`⏺ Agent(...)`として即座に表示される。
3. `write`/`edit`が、変更前後の内容から計算した実際のdiff(コンテキスト行付き、追加/削除の色分け)を表示する。40行超は切り詰められる。
4. `polaris-cli`(一発実行・headless)の既存テスト・既存挙動に一切影響がない。
5. サブエージェント内部のツール呼び出しはこのイベント経路に乗らない(深さ1の原則を維持)。

## 見送った代替案

- **コールバックtrait方式**: channelより柔軟だが、非同期コード内での呼び出しタイミング・ライフタイム管理が複雑になるため見送った。
- **`git diff`へのshell-out(既存の`/diff`と同じ手法)**: git管理外のファイル・新規ファイルに使えず、パフォーマンス上も毎ツール呼び出しでプロセス起動は避けたいため、`similar`クレートによるインプロセス計算を採用した。
