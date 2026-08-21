# polaris TUI 初回起動オンボーディング画面 設計

日付: 2026-08-21
ステータス: ドラフト(ユーザーレビュー待ち)

## 背景

`POLARIS_API_KEY` 未設定・`polaris login` 未実施のまま `polaris`(対話TUI)を実行すると、`POLARIS_API_KEY is not set` と表示してそのまま終了する。codex本家のように、起動時に資格情報が無ければ「ChatGPTでサインインするか、APIキーを入力するか」を選ばせる画面を出し、その場で設定を完了できるようにする。

バージョンアップの対象ではない(git tagは打たない)。

## スコープ

含む:
- 対話TUI起動時(`--prompt` 省略)、かつ実端末(TTY)、かつ既存の解決ロジックで有効なプロバイダを組み立てられない場合に、選択画面を表示する
- 選択肢1: ChatGPTでサインイン(既存の `polaris login` と同じOAuthフロー)
- 選択肢2: OpenAI APIキーを入力し、その場で使用開始(再実行不要)。新規ファイル `~/.polaris/api_key.json` に0600で保存する
- 1/2キーおよび↑↓+Enterでの選択

含まない:
- 一発実行(`polaris -p "..."`)での対話的プロンプト化。今まで通り即エラーで終了する(スクリプト・CI用途を壊さない)
- `Credentials`/`~/.polaris/auth.json` の形式変更(既存ユーザーのファイルに一切触れない)
- codexのTUIの文言・画面デザインをそのまま複製すること(著作権・ブランディング上、polaris独自の文言で書く)

## アーキテクチャ

### 発火条件

`polaris-cli::main()` で、`args.prompt.is_none()` かつ標準入出力がTTYの場合に限り、現行のプロバイダ解決ロジック(`POLARIS_PROVIDER` の値に応じて `POLARIS_API_KEY` またはcodexの保存済み認証情報を見る)を試みる。それが失敗する場合(APIキー無し、または未ログイン)、エラーで終了する代わりにオンボーディング画面へ進む。`POLARIS_PROVIDER` が明示されているかどうかは問わない——「その場で直す」という要望に合わせ、資格情報が揃っていない状態そのものをトリガーにする。

一発実行(`--prompt` 明示時)は非TTY環境(CI等)を含めて動く必要があるため、この画面には一切入らず、現行通り即エラーで終了する。

### 画面遷移

```
+------------------------------------------------+
| polaris — a minimal-context coding agent harness|
|                                                  |
| Sign in to continue.                            |
|                                                  |
|   1. Sign in with ChatGPT                        |
|   2. Provide an OpenAI API key                   |
|                                                  |
| Use ↑/↓ or 1/2, then Enter.                      |
+------------------------------------------------+
```

- **選択肢1(ChatGPTサインイン)**: ラタチュイのalternate screen/raw modeを一旦抜け(`ratatui::restore()`)、既存の `polaris_auth::login::run(ISSUER, &store_path)` をそのまま呼ぶ(ブラウザを開き、OAuthコールバックを待ち、成功したら `store::save_to` で `~/.polaris/auth.json` へ保存するところまで既存関数がすべて行う)。完了後 `ratatui::init()` で再度TUIへ入り、`POLARIS_PROVIDER=codex` 相当のプロバイダを新たに組み立てて通常のチャットループへ進む。失敗した場合はオンボーディング画面へ戻り、エラーメッセージを表示する。
- **選択肢2(APIキー入力)**: 入力欄を表示(マスク表示。入力文字をそのまま出さず `*` 等で伏せる)。Enterで確定したら、新設の `polaris_auth::api_key::save_to(&path, &key)` で `~/.polaris/api_key.json` に0600で保存し、そのままそのキーでopenaiプロバイダを組み立てて通常のチャットループへ進む(保存後にプロセスを再起動する必要はない)。

### プロバイダ解決の変更点

`openai` 経路のAPIキー探索を「`POLARIS_API_KEY` 環境変数 → 無ければ `~/.polaris/api_key.json`」の2段階にする(現状は環境変数のみ)。一発実行・対話TUIどちらの経路でも、この2段階探索を使う。オンボーディング画面が新規に保存したキーも、次回起動時からはこの2段階探索で環境変数無しに拾われる。

### 保存形式

新設 `polaris-auth::api_key` モジュール:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    pub key: String,
}

pub fn default_path() -> Result<PathBuf, AuthError>; // ~/.polaris/api_key.json
pub fn save_to(path: &Path, key: &str) -> Result<(), AuthError>;
pub fn load_from(path: &Path) -> Result<Option<String>, AuthError>;
```

`store.rs` の `save_to`/`load_from`(tmp+rename、0600、set_permissionsによる二重保証)と同じ実装パターンをそのまま踏襲する。`Credentials`・`~/.polaris/auth.json` には一切触れない——既存ユーザーのファイルへの影響はゼロ。

## エラーハンドリング

- ChatGPTサインインの途中(ブラウザでの認可待ち等)で失敗した場合、オンボーディング画面へ戻り、エラー内容を1行表示してから再度選択を促す(プロセスは終了しない)
- APIキー保存の書き込みに失敗した場合も同様に画面へ戻り、エラーを表示する
- どちらの経路でも、TUI全体を異常終了させない(この画面自体の失敗は、通常のチャットループの `Status::Error` と同じ「エラーを見せて継続」の思想を踏襲する)

## テスト方針

- `polaris-auth::api_key`: `store.rs` と同じテストパターン(保存→読み込みの往復、0600権限、存在しないファイルはNoneでエラーではない、壊れたファイルはエラー)
- APIキー探索の2段階化: 環境変数優先・環境変数無しでファイルから読む・両方無しでNone、の3パターンをユニットテストで確認
- オンボーディング画面のレンダリング(選択肢のハイライト、キー入力でのフォーカス移動)は既存の `polaris-tui` の `TestBackend` パターンで検証する
- 実際のOAuthブラウザフロー・実際のAPIキー入力操作は、v0.3.0/v0.4.0と同様に自動テストの対象外とし、README に手動確認手順を追記する

## 未解決/次工程で決めること

- APIキー入力欄のマスク文字・具体的なキー割り当て(Backspace、貼り付け対応の要否)は実装時に決める
- オンボーディング画面のcolor/styleは既存の `render.rs` の配色方針(ロール別色分け)に合わせる程度とし、詳細は実装時に決める
