# テスト

## 通常の検査

```sh
cargo fmt --all -- --check
cargo test --locked --offline -p polaris-core budget
cargo test --locked --offline -p polaris-cli tool_memory_defaults_off_and_is_independent_of_remember
```

基底コンテキストの検査は、workflow本文を除くOpenAI・Codexワイヤ形式のトークン数が990以下であること、ツール数が6以下であること、skill数の増加で固定部分が増えないことを確認する。workflow本文と識別情報は別途384トークン以下を確認する。CLIの設定テストは`--tool-memory`が既定offで、`--remember`から独立していることを確認する。

変更範囲に応じて、対象crateのテスト、`cargo clippy --locked --offline --all-targets -- -D warnings`、Python測定スクリプトのテストを追加する。ネットワーク認証、実端末の描画、実モデル推論は自動テストだけで確認できない。

## 現在の候補と検証記録

[リリース資料](releases/v1.0.0.md#検証結果)で、最新候補の確認と以前のバイナリの結果を区別する。全workspaceの並列試験は既知の失敗1件があり、単独再試験の成功を全件成功へ読み替えない。OS PTYの合成応答は実端末描画・実認証・実モデルの受入とは別に扱う。

## TUIの手動確認

v0.11.0のworkflow、strict10、再開・分岐、モデル要求の上限制御は[v0.11.0の検証状況](sadalmelik-validation.md)にまとめる。疑似応答の制御試験は実モデルの品質やトークン削減を証明しない。

資格情報のない専用HOMEとプロジェクトディレクトリから`polaris`を起動し、次を確認する。実アカウントでログアウトや秘密入力の試験を代用しない。ホームを作業ディレクトリにした場合の制約は[起動時のトラブル](usage.md#起動時のトラブル)を参照。

- `POLARIS_API_KEY`、`~/.polaris/api_key.json`、`~/.polaris/auth.json`が無い場合に選択画面が出る。
- `↑`、`↓`、`1`、`2`で選択を変え、Enterで確定できる。
- APIキー入力は伏字で表示され、確定後に再起動せず対話を開始し、`~/.polaris/api_key.json`を0600で保存する。
- ChatGPTサインインはブラウザを開き、認可後に対話を開始する。
- 文字入力、Backspace、Enterで送信でき、応答が履歴に追加される。
- 書き込み要求で承認モーダルが出て、`y`と`n`で応答できる。
- `Ctrl-C`で終了できる。workflowを使う送信済み会話は保存領域の`sessions-v2/`に保存され、`/resume`で選んで再開できる。
- ヘッダーとステータスにプロバイダ、モデル、累積トークンが表示される。
- ツール呼び出しでは`read(...)`などの操作名と結果が表示される。
- 太字、インラインコード、コードブロックが整形表示される。

- ログイン・承認・候補選択・実行中にも、表示文字をドラッグしてコピーできる。非表示の秘密値は含まれない。
- 認証URLはEnter/`o`で開き、`c`でクエリ全文をコピーできる。Ctrl-clickでは文字範囲だけが反応し、通常のclick/dragでは開かない。Terminal.appのCmd＋単クリックは未対応。
- 合成認証に対する`/logout`は追加確認なしで解除し、結果を表示する。


旧来の「同一ディレクトリで1会話を自動再開する」「`tui-session.jsonl`が作られる」という確認項目は、複数会話を`/resume`から選択する現行実装と一致しないため使わない。
