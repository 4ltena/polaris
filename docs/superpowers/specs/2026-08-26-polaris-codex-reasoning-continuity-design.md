# polaris Codexプロバイダ reasoning item保持 設計書

## 背景・目的

Codexプロバイダの`Folder::take_item`(`crates/polaris-provider/src/codex.rs`)は、SSEの`response.output_item.done`イベントのうち`"message"`と`"function_call"`しか拾わず、`"reasoning"`は`_ => {}`で捨てている。`Message`型(`crates/polaris-provider/src/lib.rs`)にも`CompletionResponse`にも、これを保持する場所が無い。

upstream `codex-rs`の`core/src/client.rs`を確認したところ、非Azureのバックエンド(polarisが使うChatGPTサブスク経路と同じ)では`store`は常に`false`で、その場合`prepare_response_items_for_request`は各itemの`id`だけを取り除いて送る。`Reasoning` item自体(`encrypted_content`込み)は取り除かれず、`input`配列の一部として毎ターン送られ続ける。有効期限や「直前ターンのみ」という制約は見当たらず、`core/src/context_manager/history.rs`の`get_non_last_reasoning_items_tokens`も、reasoning itemがセッション全体を通じて保持・再送される前提でトークン量を数えている。つまりupstreamでは、reasoning itemも他の履歴item(message、function_call)と同格の一級市民として扱われている。

**この変更の動機はキャッシュ効率ではない。** `v0.7.0`のA/B実測(`docs/superpowers/CURRENT.md`、CHANGELOG参照)で、`prompt_cache_key`も`include`も無い状態でもキャッシュ命中率は3ターン目以降93〜97%に達しており、reasoning item保持で埋めようとしていた「ツール呼び出しを跨いだ連続性の欠落」が、実際にキャッシュ効率に効いているという証拠は無い。動機はupstream `codex-rs`との挙動の一致度を高めることであり、優先度は低い。

## スコープ

対象: `polaris-provider`の`Message`型・`CompletionResponse`型・`Folder`・Codexプロバイダの`input_items`、`polaris-core`の`Session`。

非対象:
- `openai.rs`(Chat Completions APIにreasoning itemの概念が無いため、`Message.reasoning`は単に無視されるだけで新規ロジックは持たない)。
- reasoning内容そのものをTUIに表示する機能。`encrypted_content`はサーバー側で暗号化された不透明なバイト列で、人間が読める内容ではない。
- 会話履歴のコンパクション・トリミング。polarisの`Session`は現状「M1: 永続化・圧縮なしの追記のみ」(`session.rs`冒頭のコメント)で、これは本設計の対象外の既存の制約であり、reasoning item保持によって新たに悪化するわけではない(そのまま`Session.messages`が伸び続ける前提に乗る)。

## アーキテクチャ

```
1ターン目のSSE: response.output_item.done(type=reasoning, encrypted_content=...)
                → Folder.reasoning に蓄積
                → Folder::finish() で CompletionResponse.reasoning へ
                → run_loop が Session::push_assistant_tool_calls(..., reasoning) で
                  Message.reasoning へ格納

2ターン目のリクエスト構築: input_items(&session.messages) が
                Message.reasoning を、そのターンの function_call/message
                項目より前の位置に {"type":"reasoning", "id":..., "summary":[],
                "encrypted_content":...} として展開する
```

### 1. データ型(`crates/polaris-provider/src/lib.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningItem {
    pub id: String,
    pub encrypted_content: String,
}
```

`Message`に追加:

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub reasoning: Vec<ReasoningItem>,
```

既存の`Message::user`/`assistant`/`assistant_with_tool_calls`/`tool_result`はいずれも`reasoning: Vec::new()`で初期化する(呼び出し元シグネチャは変えない)。reasoningを持たせたい場合のために、チェーン可能なセッターを追加する:

```rust
pub fn with_reasoning(mut self, reasoning: Vec<ReasoningItem>) -> Self {
    self.reasoning = reasoning;
    self
}
```

`CompletionResponse`に追加:

```rust
pub reasoning: Vec<ReasoningItem>,
```

(`#[derive(Default)]`のままで良い — `Vec`は`Default`実装済み)

### 2. `Folder`(`crates/polaris-provider/src/codex.rs`)

- `Folder`構造体に`reasoning: Vec<ReasoningItem>`フィールドを追加、`new()`で空初期化。
- `take_item`に`"reasoning"`の分岐を追加する:

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
    // id か encrypted_content が欠けている場合(include が効かなかった、
    // 非reasoningモデル等)は静かに読み飛ばす。継続性はベストエフォートの
    // 最適化であり、無ければ無いまま次のターンへ進んで構わない。
}
```

- `finish()`が返す`CompletionResponse`に`reasoning: self.reasoning`を追加する。

### 3. `input_items`(`crates/polaris-provider/src/codex.rs`)

`Role::Assistant`の分岐の先頭で、そのメッセージが持つ`reasoning`を先に展開してから、既存のcontent/tool_callsの展開を続ける:

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
        out.push(serde_json::json!({ /* 既存のまま */ }));
    }
    for c in &m.tool_calls {
        out.push(serde_json::json!({ /* 既存のまま */ }));
    }
}
```

`"summary": []`は固定の空配列を送る。upstreamのワイヤ形式で`summary`は必須フィールド(`Vec`、`skip_serializing_if`無し)だが、polaris側はsummary内容自体を取得していないため空でよい。

### 4. `Session`(`crates/polaris-core/src/session.rs`)

既存の`push_assistant`/`push_assistant_tool_calls`のシグネチャを直接変更する(未使用の後方互換オーバーロードは作らない、既存呼び出し元を合わせて更新する):

```rust
pub fn push_assistant(&mut self, content: &str, reasoning: Vec<polaris_provider::ReasoningItem>) {
    self.messages.push(Message::assistant(content).with_reasoning(reasoning));
}

pub fn push_assistant_tool_calls(
    &mut self,
    content: &str,
    tool_calls: Vec<ToolCall>,
    reasoning: Vec<polaris_provider::ReasoningItem>,
) {
    self.messages
        .push(Message::assistant_with_tool_calls(content, tool_calls).with_reasoning(reasoning));
}
```

`crates/polaris-core/src/agent.rs`の`run_loop`内、この2つの呼び出し箇所を更新する。どちらの分岐でも`res.reasoning`は以降読み直されない(`res.tool_calls`と違い、後段の`for call in &res.tool_calls`のようなループ対象になっていない)ため、`.clone()`せずそのまま`res.reasoning`を渡す(ムーブ)。

- `res.tool_calls.is_empty()`の分岐: `session.push_assistant(&res.text, res.reasoning)` → その後の`Ok(AgentOutcome { text: res.text, usage })`は`res.text`のみを使う部分ムーブなので問題ない。
- ツール呼び出しがある分岐: `session.push_assistant_tool_calls(&res.text, res.tool_calls.clone(), res.reasoning)`(`res.tool_calls`は既存通り後段のループのためclone、`res.reasoning`は以降未使用なのでムーブ)。

### 5. `openai.rs`

コード変更なし。Chat Completions向けのメッセージ変換処理は`Message.content`/`tool_calls`のみを読んでおり、`reasoning`フィールドの追加によって自動的に無視される。テスト方針に、この「無視されること」自体を確認する回帰テストを含める。

## エラー処理

`encrypted_content`または`id`が欠けている`reasoning` itemはスキップする(上記)。`function_call`のid/name必須チェック(`ProviderError::Decode`を返す)ほど厳格にはしない — reasoning保持は失敗してもターン自体は成立する。

## テスト方針

1. `Folder::take_item`が`"reasoning"`type・`encrypted_content`ありのitemを`self.reasoning`へ追加すること。`encrypted_content`または`id`欠如時はスキップされ、`reasoning`が空のままであること。
2. `Folder::finish`が`self.reasoning`を`CompletionResponse.reasoning`へそのまま引き渡すこと。
3. `input_items`が、`reasoning`を持つ`Role::Assistant`の`Message`から、対応する`function_call`/`message`項目より前の位置に`{"type":"reasoning",...}`を正しい個数で出力すること。
4. `reasoning`が空の`Message`では、`input_items`の出力形状が変更前と変わらないこと(既存テストの回帰確認)。
5. `openai.rs`側の変換ロジックが`Message.reasoning`を無視し、既存の出力形状を変えないことを確認する回帰テスト。
6. `agent::run_loop`のテストフィクスチャを拡張し、`res.reasoning`が次のターンの`session.messages`へ正しく積まれることを確認する。

## 受け入れ基準

1. Codexプロバイダが`response.output_item.done`で`reasoning`typeのitemを受け取ったとき、`encrypted_content`が保持され、次のターンのリクエストに再送される。
2. 再送される`reasoning` itemは、そのターンの`function_call`/`message`項目より前の位置に置かれる。
3. `encrypted_content`が無い場合でもクラッシュ・エラーにならず、単に何も保持しない。
4. OpenAIプロバイダ(Chat Completions)の出力形状・既存テストに影響がない。
5. `cargo test --workspace`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo fmt --all -- --check`が通る。

## 見送った代替案

- **`Message`/`Session`とは別の側路(provider-local side channel)で保持する案**: `Session`/`Message`という共有データ型に触れずに済むが、`CodexProvider`内部で独自にターンインデックスを追跡し`Session.messages`と同期させ続ける必要がある。`tool_calls`がすでに同じ性質の情報を`Message`型自体に持たせて解決しており、既存パターンとの一貫性を優先してこの案は見送った。
- **ターン内のreasoningとtool_callsの厳密な出力順(interleaving)を保持する案**: upstreamへのより忠実な再現にはなるが、既存の`tool_calls`集約自体がすでにこの順序情報を捨てており(1ターン分をフラットな`Vec`にまとめる)、reasoningだけ精度を上げても実益が薄い。既存の粒度に合わせ見送った。
- **直近1ターン分の`reasoning`のみ保持する案**: upstreamの`client.rs`調査で、reasoning itemは他の履歴itemと同様セッション全体を通じて保持・再送される設計だと確認できたため、この簡略化は根拠を失った。
