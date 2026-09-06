# polaris Codexプロバイダ reasoning item保持 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Codexプロバイダが受け取る`reasoning`item(`encrypted_content`込み)を捨てずに保持し、次のターンのリクエストへ再送する。

**Architecture:** `Message`に`tool_calls`と同じパターンで`reasoning: Vec<ReasoningItem>`を追加する。Codexプロバイダの`Folder`がSSEの`response.output_item.done`(`type: "reasoning"`)を拾って`CompletionResponse.reasoning`へ運び、`Session::push_assistant`/`push_assistant_tool_calls`経由で`Message.reasoning`へ格納、`input_items`が次のターンでそれをワイヤ項目として再送する。OpenAIプロバイダ(Chat Completions)はこのフィールドを単に無視する。

**Tech Stack:** Rust、`polaris-provider`(`lib.rs`/`codex.rs`/`openai.rs`)、`polaris-core`(`session.rs`/`agent.rs`)。既存のテストは`cargo test`、フォーマットは`cargo fmt`、静的解析は`cargo clippy --all-targets -- -D warnings`。

**Spec:** `docs/superpowers/specs/2026-08-26-polaris-codex-reasoning-continuity-design.md`

## Global Constraints

- 対象は`polaris-provider`と`polaris-core`のみ。`openai.rs`はロジック変更なし(`Message.reasoning`は自動的に無視される)。
- reasoning itemの`id`または`encrypted_content`が欠けている場合は静かにスキップする(エラーにしない)。
- 既存の`Message::user`/`assistant`/`assistant_with_tool_calls`/`tool_result`のシグネチャは変更しない。reasoningを持たせたいときだけ`.with_reasoning(...)`をチェーンする。
- `Session::push_assistant`/`push_assistant_tool_calls`のシグネチャは変更する(既存の後方互換シムは作らない)。呼び出し元は`crates/polaris-core/src/agent.rs`の2箇所のみ(確認済み: `grep -rn "push_assistant" crates/polaris-core/src`)。
- 各タスクの最後に`cargo test -p <該当crate>`(該当クレートのみ)、最終タスクの最後に`cargo test --workspace`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo fmt --all -- --check`を通す。
- コミットメッセージは Conventional Commits 形式、`Co-Authored-By: Claude <noreply@anthropic.com>`を末尾に付ける。

---

### Task 1: `ReasoningItem`型と`Message`/`CompletionResponse`への保持場所

**Files:**
- Modify: `crates/polaris-provider/src/lib.rs`

**Interfaces:**
- Produces: `pub struct ReasoningItem { pub id: String, pub encrypted_content: String }`(`Debug, Clone, Serialize, Deserialize, PartialEq, Eq`導出)。`Message.reasoning: Vec<ReasoningItem>`フィールド。`Message::with_reasoning(self, reasoning: Vec<ReasoningItem>) -> Self`。`CompletionResponse.reasoning: Vec<ReasoningItem>`フィールド。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS(ベースライン確認)

- [ ] **Step 2: `Message`のserde往復テストを追加する(失敗するテストを先に書く)**

`crates/polaris-provider/src/lib.rs`の`mod tests`(ファイル末尾、既存の`Message`/`ToolCall`関連テストが無ければ新設する — 無い場合は`#[cfg(test)] mod tests { use super::*; ... }`をファイル末尾に追加)に以下を追加する:

```rust
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
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-provider a_message_with_reasoning_round_trips_through_json a_message_without_reasoning_omits_the_field_from_json`
Expected: コンパイルエラー(`ReasoningItem`が存在しない、`with_reasoning`が存在しない、`reasoning`フィールドが存在しない)

- [ ] **Step 4: `ReasoningItem`型・`Message.reasoning`・`with_reasoning`・`CompletionResponse.reasoning`を実装する**

`crates/polaris-provider/src/lib.rs`の`pub struct Message`の直前に追加:

```rust
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
```

`Message`構造体に`tool_call_id`の直後、フィールドとして追加:

```rust
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<ReasoningItem>,
```

既存の4つのコンストラクタ(`user`/`assistant`/`assistant_with_tool_calls`/`tool_result`)それぞれの`Self { ... }`に`reasoning: Vec::new(),`を追加する。

`impl Message`ブロックの末尾に追加:

```rust
    /// Attaches the reasoning items produced alongside this turn, so the
    /// Codex provider can replay them on the next request. A no-op for
    /// providers that don't carry the concept.
    pub fn with_reasoning(mut self, reasoning: Vec<ReasoningItem>) -> Self {
        self.reasoning = reasoning;
        self
    }
```

`pub struct CompletionResponse`に`tool_calls`の直後、フィールドとして追加:

```rust
    pub reasoning: Vec<ReasoningItem>,
```

(`#[derive(Debug, Clone, Default)]`はそのまま — `Vec<ReasoningItem>`は`Default`実装済み)

- [ ] **Step 5: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS(既存テストも含めて回帰なし)

- [ ] **Step 6: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-provider -- --check && cargo clippy -p polaris-provider --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-provider/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(polaris-provider): add ReasoningItem and a slot to carry it

Message and CompletionResponse gain a reasoning: Vec<ReasoningItem>
field, mirroring the existing tool_calls pattern. Nothing populates it
yet - this is the shared type both the Codex provider's capture path
and its replay path will build on.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: `Folder`が`reasoning` itemを捕捉する

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: `ReasoningItem { id: String, encrypted_content: String }`(Task 1)、`CompletionResponse.reasoning: Vec<ReasoningItem>`(Task 1)。
- Produces: `Folder`が`reasoning` typeのSSE itemを`self.reasoning: Vec<ReasoningItem>`へ蓄積し、`finish()`が`CompletionResponse.reasoning`へ引き渡す。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS(ベースライン確認、Task 1の変更込み)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-provider/src/codex.rs`の`mod tests`内、`fn function_call_item(...)`の直後に、対応するヘルパーを追加する:

```rust
fn reasoning_item(id: &str, encrypted_content: &str) -> Value {
    serde_json::json!({
        "item": {
            "type": "reasoning",
            "id": id,
            "summary": [],
            "encrypted_content": encrypted_content,
        }
    })
}
```

`fn a_function_call_item_becomes_a_tool_call()`の直後に、以下の2つのテストを追加する:

```rust
#[test]
fn a_reasoning_item_is_captured_with_its_encrypted_content() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_item.done",
        reasoning_item("r1", "opaque-blob"),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should be complete");
    assert_eq!(r.reasoning.len(), 1);
    assert_eq!(r.reasoning[0].id, "r1");
    assert_eq!(r.reasoning[0].encrypted_content, "opaque-blob");
}

#[test]
fn a_reasoning_item_without_encrypted_content_is_skipped() {
    let mut f = Folder::new();
    f.push(&frame(
        "response.output_item.done",
        serde_json::json!({ "item": { "type": "reasoning", "id": "r1", "summary": [] } }),
    ))
    .expect("push should succeed");
    f.push(&frame("response.completed", serde_json::json!({})))
        .expect("push should succeed");
    let r = f.finish().expect("should be complete");
    assert!(
        r.reasoning.is_empty(),
        "a reasoning item with no encrypted_content must not be kept"
    );
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-provider a_reasoning_item_is_captured_with_its_encrypted_content a_reasoning_item_without_encrypted_content_is_skipped`
Expected: コンパイルエラー(`CompletionResponse.reasoning`は存在するが空のまま — `r.reasoning.len()`のアサーションが`0 != 1`で失敗する。もしくは`Folder`に`reasoning`フィールドが無くビルドは通る)

- [ ] **Step 4: `Folder`に`reasoning`を捕捉するロジックを実装する**

`crates/polaris-provider/src/codex.rs`冒頭の`use crate::{...}`に`ReasoningItem`を追加する:

```rust
use crate::{
    CompletionRequest, CompletionResponse, Message, Provider, ProviderError, ReasoningItem, Role,
    ToolCall, sse,
};
```

(既存の並び順`CompletionRequest, CompletionResponse, Message, Provider, ProviderError, Role, ToolCall, sse`はそのまま保ち、`ReasoningItem`を`ProviderError`の直後・`Role`の直前に挿入するだけ)

`pub struct Folder`に`usage: Option<crate::Usage>,`の直後、フィールドを追加:

```rust
    reasoning: Vec<ReasoningItem>,
```

`impl Folder`の`pub fn new()`の`Self { ... }`に追加:

```rust
            reasoning: Vec::new(),
```

`fn take_item`の`match`式、`"function_call" => { ... }`の直後・`_ => {}`の直前に分岐を追加する:

```rust
            "reasoning" => {
                if let (Some(id), Some(encrypted_content)) = (
                    item.get("id").and_then(|v| v.as_str()),
                    item.get("encrypted_content").and_then(|v| v.as_str()),
                ) {
                    self.reasoning.push(ReasoningItem {
                        id: id.to_string(),
                        encrypted_content: encrypted_content.to_string(),
                    });
                }
                // `id`/`encrypted_content` 欠如(`include` が効かなかった、
                // reasoning非対応モデル等)は静かに読み飛ばす。継続性は
                // ベストエフォートの最適化で、無ければ無いまま次のターン
                // へ進んで構わない。
            }
```

`pub fn finish(self)`が返す`Ok(CompletionResponse { ... })`に追加:

```rust
            reasoning: self.reasoning,
```

- [ ] **Step 5: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS

- [ ] **Step 6: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-provider -- --check && cargo clippy -p polaris-provider --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-provider/src/codex.rs
git commit -m "$(cat <<'EOF'
feat(polaris-provider): capture reasoning items from the SSE stream

Folder::take_item previously fell through "reasoning" typed items to
the catch-all arm and dropped them. It now keeps id/encrypted_content
pairs and threads them out through CompletionResponse.reasoning. An
item missing either field is skipped, not an error - this capture is
best-effort.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `input_items`が`reasoning`をワイヤへ再送する

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: `Message.reasoning: Vec<ReasoningItem>`(Task 1)。
- Produces: `input_items`が`Role::Assistant`のメッセージが持つ`reasoning`を、そのターンの`message`/`function_call`項目より前に`{"type":"reasoning","id":...,"summary":[],"encrypted_content":...}`として出力する。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS(Task 1・2の変更込み)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-provider/src/codex.rs`の`mod tests`内、`fn a_turn_with_both_text_and_calls_emits_the_message_first()`の直後に追加する:

```rust
#[test]
fn a_turn_with_reasoning_emits_it_before_the_message_and_calls() {
    let items = input_items(&[Message::assistant_with_tool_calls(
        "I'll read it",
        vec![ToolCall {
            id: "c".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        }],
    )
    .with_reasoning(vec![crate::ReasoningItem {
        id: "r1".into(),
        encrypted_content: "opaque".into(),
    }])]);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[0]["id"], "r1");
    assert_eq!(items[0]["encrypted_content"], "opaque");
    assert_eq!(items[1]["type"], "message");
    assert_eq!(items[2]["type"], "function_call");
}

#[test]
fn a_turn_without_reasoning_emits_no_reasoning_item() {
    let items = input_items(&[Message::assistant("yes")]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-provider a_turn_with_reasoning_emits_it_before_the_message_and_calls`
Expected: FAIL(`items.len()`が3ではなく2 — reasoningがまだ出力されていない)

- [ ] **Step 4: `input_items`に`reasoning`の展開を実装する**

`crates/polaris-provider/src/codex.rs`の`pub fn input_items`、`Role::Assistant => { ... }`ブロックの先頭(`if !m.content.is_empty() {`より前)に追加する:

```rust
            Role::Assistant => {
                for r in &m.reasoning {
                    out.push(serde_json::json!({
                        "type": "reasoning",
                        "id": r.id,
                        "summary": [],
                        "encrypted_content": r.encrypted_content,
                    }));
                }
                if !m.content.is_empty() {
```

(残りの`if !m.content.is_empty() { ... }`と`for c in &m.tool_calls { ... }`は変更しない)

- [ ] **Step 5: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS

- [ ] **Step 6: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-provider -- --check && cargo clippy -p polaris-provider --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-provider/src/codex.rs
git commit -m "$(cat <<'EOF'
feat(polaris-provider): replay reasoning items on the next turn

input_items now emits a Message's reasoning items as {"type":
"reasoning", ...} wire elements, positioned before that turn's
message/function_call items - matching the order they were originally
emitted in. A Message with no reasoning produces no extra item, so
this is a no-op until Folder actually starts capturing them.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `Session`と`agent::run_loop`を配線する

**Files:**
- Modify: `crates/polaris-core/src/session.rs`
- Modify: `crates/polaris-core/src/agent.rs`

**Interfaces:**
- Consumes: `ReasoningItem`(Task 1、`polaris_provider::ReasoningItem`として)、`CompletionResponse.reasoning`(Task 2で実際に値が入るようになる)。
- Produces: `Session::push_assistant(&mut self, content: &str, reasoning: Vec<polaris_provider::ReasoningItem>)`、`Session::push_assistant_tool_calls(&mut self, content: &str, tool_calls: Vec<ToolCall>, reasoning: Vec<polaris_provider::ReasoningItem>)`。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全テストPASS(ベースライン確認)

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs`の`mod tests`内、`usage_accumulates_across_a_tool_calling_turn`(既存テスト、`Scripted`プロバイダを使う)のすぐ後に、以下のテストを追加する:

```rust
#[tokio::test]
async fn reasoning_from_a_tool_calling_turn_is_carried_into_the_session() {
    let dir = tempfile::tempdir().expect("temp directory");
    let target = dir.path().join("a.txt");
    std::fs::write(&target, "hello\n").expect("cannot write");

    let p = Scripted {
        replies: Mutex::new(vec![
            CompletionResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                }],
                reasoning: vec![polaris_provider::ReasoningItem {
                    id: "r1".into(),
                    encrypted_content: "opaque".into(),
                }],
                usage: None,
            },
            CompletionResponse {
                text: "it was 1 line".into(),
                ..Default::default()
            },
        ]),
    };

    let mut session = Session::new();
    session.push_user("how many lines is a.txt");
    let audit = shared_audit(&dir.path().join("audit.jsonl"));
    let mut stop = StopTracker::new(10);

    let always_on = crate::prompt::assemble_always_on("", "", &[]);
    let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper: &helper,
        gate: &mut gate,
        approver: &mut approver,
    };
    run(
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
        None,
        &mut ctx,
    )
    .await
    .expect("should succeed");

    // messages[0] is the user turn, messages[1] is the assistant turn that
    // called the tool - that's the one that must carry the reasoning item
    // captured alongside it.
    let tool_calling_turn = &session.messages[1];
    assert_eq!(tool_calling_turn.reasoning.len(), 1);
    assert_eq!(tool_calling_turn.reasoning[0].id, "r1");
    assert_eq!(tool_calling_turn.reasoning[0].encrypted_content, "opaque");
}
```

- [ ] **Step 3: テストを実行して失敗を確認する**

Run: `cargo test -p polaris-core reasoning_from_a_tool_calling_turn_is_carried_into_the_session`
Expected: コンパイルエラー(`Session::push_assistant_tool_calls`が`reasoning`引数を受け取らない)

- [ ] **Step 4: `Session`のシグネチャを変更する**

`crates/polaris-core/src/session.rs`冒頭の`use polaris_provider::{Message, ToolCall};`を次に変更する:

```rust
use polaris_provider::{Message, ReasoningItem, ToolCall};
```

`push_assistant`を次に変更する:

```rust
    pub fn push_assistant(&mut self, content: &str, reasoning: Vec<ReasoningItem>) {
        self.messages
            .push(Message::assistant(content).with_reasoning(reasoning));
    }
```

`push_assistant_tool_calls`を次に変更する:

```rust
    /// Records an assistant turn together with the tool calls it made.
    /// Under OpenAI's round-trip protocol, this message must remain in the
    /// history holding its own `tool_calls` before the tool results are sent.
    pub fn push_assistant_tool_calls(
        &mut self,
        content: &str,
        tool_calls: Vec<ToolCall>,
        reasoning: Vec<ReasoningItem>,
    ) {
        self.messages.push(
            Message::assistant_with_tool_calls(content, tool_calls).with_reasoning(reasoning),
        );
    }
```

- [ ] **Step 5: `agent::run_loop`の呼び出しを更新する**

`crates/polaris-core/src/agent.rs`の`run_loop`内:

`session.push_assistant(&res.text);`を次に変更する(`res.reasoning`は以降読み直されないのでムーブする):

```rust
            session.push_assistant(&res.text, res.reasoning);
```

`session.push_assistant_tool_calls(&res.text, res.tool_calls.clone());`を次に変更する(`res.tool_calls`は既存通り後段の`for call in &res.tool_calls`ループのためclone、`res.reasoning`は以降未使用なのでムーブ):

```rust
        session.push_assistant_tool_calls(&res.text, res.tool_calls.clone(), res.reasoning);
```

- [ ] **Step 6: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全テストPASS(`push_assistant`/`push_assistant_tool_calls`の他の呼び出し元は無い — 事前に`grep -rn "push_assistant" crates/polaris-core/src`で確認済み)

- [ ] **Step 7: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-core -- --check && cargo clippy -p polaris-core --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 8: コミット**

```bash
git add crates/polaris-core/src/session.rs crates/polaris-core/src/agent.rs
git commit -m "$(cat <<'EOF'
feat(polaris-core): thread reasoning items through Session

push_assistant/push_assistant_tool_calls now take the reasoning items
captured alongside that turn and attach them to the pushed Message via
Message::with_reasoning. run_loop's two call sites pass res.reasoning
through - the only two callers in the crate. This is the last hop:
Session.messages now carries what input_items (Task 3) already knows
how to replay.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `openai.rs`が`reasoning`を無視することを確認する回帰テスト

**Files:**
- Modify: `crates/polaris-provider/src/openai.rs`

**Interfaces:**
- Consumes: `Message.reasoning`(Task 1)、`Message::with_reasoning`(Task 1)。
- Produces: なし(既存の出力形状が変わらないことを保証する回帰テストのみ)。

- [ ] **Step 1: 現状のテストが通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全テストPASS(Task 1〜4の変更込み)

- [ ] **Step 2: テストを書く(このタスクは実装追加ではなく確認なので、テストは最初から通る想定 — それ自体が「無視されている」ことの証明になる)**

`crates/polaris-provider/src/openai.rs`の`mod tests`内、`sends_tool_definitions_in_the_shape_tool_wire_shape_produces`の直後に追加する:

```rust
#[tokio::test]
async fn a_message_with_reasoning_is_sent_unchanged_since_chat_completions_has_no_such_concept() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{ "message": { "content": "ok" } }]
        })))
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into())
        .expect("client should be constructible");
    let with_reasoning = Message::assistant("yes").with_reasoning(vec![crate::ReasoningItem {
        id: "r1".into(),
        encrypted_content: "opaque".into(),
    }]);
    let without_reasoning = Message::assistant("yes");

    p.complete(CompletionRequest {
        system: "s".into(),
        messages: vec![with_reasoning],
        tools: vec![],
    })
    .await
    .expect("should succeed");
    p.complete(CompletionRequest {
        system: "s".into(),
        messages: vec![without_reasoning],
        tools: vec![],
    })
    .await
    .expect("should succeed");

    let received = server.received_requests().await.expect("recorded");
    let with_body: Value = received[0].body_json().expect("json");
    let without_body: Value = received[1].body_json().expect("json");
    assert_eq!(
        with_body["messages"], without_body["messages"],
        "a Message's reasoning field must not change the Chat Completions request body"
    );
}
```

- [ ] **Step 3: テストを実行する**

Run: `cargo test -p polaris-provider a_message_with_reasoning_is_sent_unchanged_since_chat_completions_has_no_such_concept`
Expected: PASS(既にTask 1〜4のどの変更も`openai.rs`のロジックに触れていないため、これは最初からPASSする — これ自体がスコープ外だという主張の裏付けになる)

- [ ] **Step 4: フォーマット・静的解析を確認する**

Run: `cargo fmt -p polaris-provider -- --check && cargo clippy -p polaris-provider --all-targets -- -D warnings`
Expected: どちらもクリーン

- [ ] **Step 5: コミット**

```bash
git add crates/polaris-provider/src/openai.rs
git commit -m "$(cat <<'EOF'
test(polaris-provider): confirm Chat Completions ignores reasoning

Message.reasoning (Task 1) has no Chat Completions equivalent, and
openai.rs's request-building code never reads it - this test pins
that down so a future refactor can't accidentally start leaking it
into the wrong wire shape.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: ワークスペース全体の最終検証

**Files:** なし(検証のみ)

**Interfaces:** なし

- [ ] **Step 1: ワークスペース全体のテストを実行する**

Run: `cargo test --workspace`
Expected: 全テストPASS

- [ ] **Step 2: 静的解析を実行する**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: クリーン

- [ ] **Step 3: フォーマットを確認する**

Run: `cargo fmt --all -- --check`
Expected: クリーン

- [ ] **Step 4: 手動での実地確認(任意、実施した場合は記録する)**

`POLARIS_DUMP_USAGE=1 ./target/release/polaris exec "<複数ターンになるタスク>"`を実行し、2ターン目以降のリクエストで`reasoning`itemが正しく再送されていることを、必要なら一時的な`eprintln!`かデバッガで確認する。恒久的なデバッグ出力の追加はこのタスクの対象外。

- [ ] **Step 5: `docs/superpowers/CURRENT.md`を更新する**

「プロンプトキャッシュの利用率」節の末尾、または新しい節として、この機能が実装・検証済みであることを記録する。優先度が低い(upstream一致度が動機、キャッシュ効率の裏付けは無い)ことを明記したままにする。

- [ ] **Step 6: コミット**

```bash
git add docs/superpowers/CURRENT.md
git commit -m "$(cat <<'EOF'
docs: record reasoning-item continuity as implemented

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```
