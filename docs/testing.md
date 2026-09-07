# テスト

## 通常の検査

```sh
cargo fmt --all -- --check
cargo test --locked --offline -p polaris-core budget
cargo test --locked --offline -p polaris-cli tool_memory_defaults_off_and_is_independent_of_remember
```

常時コンテキストの検査は、実際のOpenAI・Codexワイヤ形式のトークン数が990以下であること、ツール数が6以下であること、skill数の増加で固定部分が増えないことを確認する。CLIの設定テストは`--tool-memory`が既定offで、`--remember`から独立していることを確認する。

変更範囲に応じて、対象crateのテスト、`cargo clippy --locked --offline --all-targets -- -D warnings`、Python測定スクリプトのテストを追加する。ネットワーク認証、実端末の描画、実モデル推論は自動テストだけで確認できない。

## TUIの手動確認

資格情報のない検証用環境で`polaris`を起動し、次を確認する。

- `POLARIS_API_KEY`、`~/.polaris/api_key.json`、`~/.polaris/auth.json`が無い場合に選択画面が出る。
- `↑`、`↓`、`1`、`2`で選択を変え、Enterで確定できる。
- APIキー入力は伏字で表示され、確定後に再起動せず対話を開始し、`~/.polaris/api_key.json`を0600で保存する。
- ChatGPTサインインはブラウザを開き、認可後に対話を開始する。
- 文字入力、Backspace、Enterで送信でき、応答が履歴に追加される。
- 書き込み要求で承認モーダルが出て、`y`と`n`で応答できる。
- `Ctrl-C`で終了できる。送信済み会話は`~/.polaris/sessions/`に保存され、`/resume`で選んで再開できる。
- ヘッダーとステータスにプロバイダ、モデル、累積トークンが表示される。
- ツール呼び出しでは`⚙ read(...)`と`→ ...`が表示される。
- 太字、インラインコード、コードブロックが整形表示される。

旧来の「同一ディレクトリで1会話を自動再開する」「`tui-session.jsonl`が作られる」という確認項目は、複数会話を`/resume`から選択する現行実装と一致しないため使わない。
