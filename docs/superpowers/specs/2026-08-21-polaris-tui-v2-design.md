# polaris TUI v2 設計

日付: 2026-08-21
ステータス: ドラフト(ユーザーレビュー待ち)
版: `v0.4.0`(通称 `Regulus`)に含める予定。`Regulus` は版番号(`v0.4.0`)自体に付く通称であり、TUI v2 単体には付けない——同じ `v0.4.0` に TUI onboarding も含める。詳細は `docs/superpowers/CURRENT.md` を参照

## 背景

v0.3.0 “Castor” で対話TUIの最小構成(会話履歴・入力欄・承認モーダル・セッション永続化)を出荷した。実際に使ってみたところ「要素が少なすぎて見づらい」というフィードバックを得た。codex(openai/codex)のTUIを参考に、以下4点を検討した:

1. ヘッダー/ステータスバー(モデル名・プロバイダー・トークン使用量を常時表示)
2. ツール実行の可視化(どのツールをいつ呼んだか・結果の要約をインライン表示)
3. 色付け・マークダウン整形(ロールごとの色分け、コードブロックの整形)
4. 進行状況ツリー(codex-orchestia/Claude Codeの`/workflows`のような階層表示)

4点目は明示的に**見送り**が確定している。polarisは現状単一エージェント(M4のsubagent波オーケストレーションは未実装)であり、表示すべき階層構造が実在しない。M4完了後に別途検討する。

## スコープ

含む:
- トークン使用量(累積)の取得と常時表示
- ツール呼び出し・結果のインライン表示(振り返り表示。リアルタイムのコールバックは追加しない)
- ロール別の色分けと、太字・インラインコード・フェンス付きコードブロックの最小限のマークダウン整形

含まない:
- 進行状況ツリー(多階層表示) — M4完了後
- リアルタイムのツール実行通知(スピナーのアニメーション等) — v0.3.0設計時に不採用と決めた案Bの領域のまま
- 見出し・リスト・リンクなどマークダウンの他の要素

## アーキテクチャ

### トークン使用量

現状 `polaris_provider::CompletionResponse` は `text` と `tool_calls` のみを持ち、両プロバイダとも実際のAPI応答に含まれる `usage` フィールドを一切パースしていない。

`polaris-provider` に以下を追加する:

```rust
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}
```

`CompletionResponse` に `pub usage: Option<Usage>` を追加する。`None` は「取得できなかった」ことを表し、ハードエラーにはしない(既存のテストフィクスチャの多くが `usage` を含まないため、必須化すると無関係な既存テストを壊す)。

- `openai.rs`: レスポンスJSONの `usage.prompt_tokens` → `input_tokens`、`usage.completion_tokens` → `output_tokens`、`usage.total_tokens` → `total_tokens` に対応させる。`usage` フィールド自体が無い/形式が崩れている場合は `None` を返す(既存の `content`/`tool_calls` の必須フィールドと違い、致命的エラーにしない)。
- `codex.rs`: SSEストリームの `response.completed` イベントのペイロード(`/response/usage` 付近、`input_tokens`/`output_tokens`/`total_tokens`)から取得する。`Folder` に `usage: Option<Usage>` フィールドを足し、`response.completed` を観測した時点で埋める。

### `agent::run` の戻り値変更

`agent::run` は1ターンの中でツール呼び出しがあれば複数回 `provider.complete()` を呼ぶ。ターン全体の使用量を呼び出し側(TUI)へ返すため、戻り値を変更する:

```rust
// 変更前
pub async fn run(...) -> Result<String, AgentError>

// 変更後
pub struct AgentOutcome {
    pub text: String,
    pub usage: Usage,
}
pub async fn run(...) -> Result<AgentOutcome, AgentError>
```

`usage` はターン内の各 `provider.complete()` 呼び出しが返した `Usage`(`None` の場合は加算しない)を単純加算した累積値。これは破壊的変更であり、影響範囲は把握済み:

- `polaris-core/src/agent.rs` 内のテスト(直接 `agent::run` を呼んでいるもの、約12箇所)
- `polaris-cli/src/main.rs` の一発実行パス(`Ok(text) => println!("{text}")` を `Ok(outcome) => println!("{}", outcome.text)` に変更)
- `polaris-tui/src/lib.rs` の `run()`(`Ok(_)` 分岐で `outcome.usage` を累積カウンタへ加算する)

この変更は「常時コンテキストを990トークン以下に保つ」という予算方針(`crates/polaris-core/src/budget.rs`)には触れない。`Usage` はモデル応答の事後報告であり、次のターンへ送るプロンプトのサイズ決定には一切使わない。

### ツール呼び出しの可視化(振り返り表示)

`agent::run` にコールバック/オブザーバーは追加しない。`Session` には既にツール呼び出し(`push_assistant_tool_calls`)とツール結果(`push_tool_result`)が記録されている。`polaris-tui/src/render.rs` の `history_lines` は現在 `Role::Tool` を除外描画しているだけなので、この除外をやめて整形する:

- assistant メッセージの `tool_calls` が非空の場合、各呼び出しを `⚙ {name}({短縮した引数})` として1行表示する
- 直後に続く `Role::Tool` メッセージの `content` は、そのまま全文を出すと長大になりうる(例: `read` ツールの出力)ため、表示用に先頭一定文字数(200文字目安)で打ち切って `→ {要約}...` として表示する。**打ち切るのは表示のみ**であり、`Session.messages` 自体(モデルへ送る内容)は変更しない
- これらの新規描画パスも既存の `sanitize()` を通す(ツール名・引数・結果はすべて外部由来コンテンツであり、v0.3.0のセキュリティ修正の対象範囲を継続する)

「いつ呼んだか」は実時刻ではなく、履歴内の出現順序(既存のメッセージ順)で表現する。ターン全体が同期的にブロッキング処理される現在のアーキテクチャ(v0.3.0で採用した案A)と一致する——ツールが呼ばれた瞬間ではなく、ターン完了時にまとめて明らかになる。

### ステータスバー

`render_chat` の上部(または現在の履歴ペイン直上)に1行のステータスバーを追加する。表示内容: プロバイダー名・モデル名・累積トークン使用量(input/output/total)。

`RunArgs` に `provider_name: String` と `model_name: String` を追加する(`polaris-cli::main()` は既にこれらをローカル変数として持っている)。`run()` のループはターンが成功するたびに `AgentOutcome.usage` を累積カウンタ(`Usage`)へ加算し、次の描画で反映する。

### 色付け・マークダウン整形

新規の重量級依存(`pulldown-cmark` 等)は追加しない。`render.rs` に以下だけを認識する最小限のインライン整形を追加する:

- `**太字**` → `Modifier::BOLD`
- `` `インラインコード` `` → 背景色を変えた `Span`
- ` ``` ` で囲まれたフェンス付きコードブロック → 行全体の背景色を変える

ロール別の色分け(user/assistant/tool)は `ratatui::style::Color` で対応する。`Line::from(String)` ベースの現在の実装を、複数の `Span` を持つ `Line` を組み立てる形へ変更する。パーサは正規表現や外部crateを使わず、単純な文字列走査で実装する(スコープが3パターンのみのため)。

## エラーハンドリング

- `Usage` の取得失敗(`None`)はエラーではない。ステータスバーは「取得できなかった」ことを示す表示(例: `usage: n/a`)に留め、TUI全体を止めない
- マークダウンの不正な構文(閉じていない `` ` `` や ` ``` ` など)はパース失敗として扱わず、そのまま生テキストとして表示する(ベストエフォート。エラーにしない)

## テスト方針

- `Usage` のパース: `openai.rs`/`codex.rs` それぞれに、`usage` ありのレスポンス・なしのレスポンス・壊れたレスポンスでのテストを追加する
- `agent::run` の戻り値変更: 既存テストを `AgentOutcome` 形に更新し、複数ターン(ツール呼び出しあり)で `usage` が正しく積算されることを検証するテストを追加する
- `render.rs`: ツール呼び出し・結果の表示、ステータスバーの表示内容、太字/インラインコード/コードブロックそれぞれの整形を、既存の `TestBackend` パターンで検証する
- 打ち切り(200文字目安)の境界値テスト、`sanitize()` が新規描画パス全てを通ることを確認するテスト(v0.3.0のセキュリティ修正と同じパターン)

## 未解決/次工程で決めること

- ツール結果の打ち切り文字数(200文字は目安であり、実装計画側で確定する)
- ステータスバーのレイアウト詳細(1行に収まらない場合の省略方法)は実装時に決める
