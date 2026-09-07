# 利用ガイド

## ビルドと一発実行

Rust 1.96.0とedition 2024を使う。

```sh
cargo build --release
POLARIS_API_KEY=sk-... target/release/polaris -p "Cargo.toml は何行か"
```

リリースビルド前は`cargo run -p polaris-cli --`の後ろに同じ引数を続けられる。`exec [PROMPT]`は`--prompt`/`-p`と同じone-shot実行の別名であり、引数を省略するか`-`を渡すと標準入力から指示を読む。

| 変数 | 既定値 | 意味 |
| --- | --- | --- |
| `POLARIS_PROVIDER` | `openai` | `openai`または`codex`。`codex`は`polaris login`で得たChatGPT認証を使う。 |
| `POLARIS_API_KEY` | なし | `POLARIS_PROVIDER=openai`で必要なOpenAI互換エンドポイントのAPIキー。 |
| `POLARIS_BASE_URL` | `https://api.openai.com/v1` | `openai`プロバイダのベースURL。 |
| `POLARIS_MODEL` | `gpt-6-astra` | 両プロバイダ共通の既定モデル。明示した`--model`が優先する。 |

`POLARIS_CACHE_PACING`の既定は`off`。`on`は同じCodex providerのモデル要求開始を最低5秒間隔にする実験設定で、完了までの時間が伸びる場合がある。[比較条件と結果](gpt6-cache-pacing-results.md)を参照。

推論強度の既定は`medium`。APIにはモデル名を`gpt-6-astra`、推論強度を`medium`として別々に渡す。`--effort`で明示した値が優先され、アカウントのプランによって既定値は変わらない。

`polaris sandbox -- <コマンド...>`はエージェントループを介さず、選択中のサンドボックスポリシーでコマンドを実行し、終了コードと標準出力・標準エラーを返す。`polaris doctor`は保存済み資格情報、既定プロバイダ、サンドボックスの可否を読み取り専用で診断し、鍵の中身は表示しない。`polaris completion <bash|zsh|fish|elvish|powershell>`は補完スクリプトを標準出力へ生成する。

## 監査ログと記憶

`--audit <path>`で監査ログの出力先を指定できる。省略時は`~/.polaris/state/<project-id>/audit.jsonl`へ追記する。`<project-id>`はcanonicalize済みの作業ディレクトリから求め、リポジトリの外に置く。各ツール呼び出しを1行のJSONとして記録し、書き込み前に秘密情報を伏字化する。

`--tool-memory`の既定は`off`である。`history`は直近2組の完了済みツール往復を保持し、それ以前の8KiB以上のread・bash結果を保存して短い参照へ置き換える。`retrieval`は大きな完了結果を初回送信前から参照と小抜粋へ置き換える。保存失敗時は全文を維持する。圧縮前履歴を保存する`--remember`とは独立している。保存、検索URI、埋め込み、ベンチマーク手順は[コンテキスト効率化](context-efficiency.md)にある。

## ChatGPT認証

```sh
polaris login
POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"
```

`polaris login`はブラウザを開き、`http://localhost:1455/auth/callback`で認可を受け取る。資格情報は`~/.polaris/auth.json`に0600で保管し、`~/.codex/`には読み書きしない。`polaris logout`はPolarisが保存した資格情報を削除する。`ls -l ~/.polaris/auth.json`で`-rw-------`であることを確認できる。

期限切れの更新経路を手で確認する場合は、テスト用の資格情報に限り`expires_at`を過去のUnix秒または`null`へ変更し、再実行後に`expires_at`、`account_id`、0600が維持されることを確認する。JSONを壊さないこと。

## TUIと会話

`--prompt`を省略すると対話TUIを起動する。資格情報がなく、`POLARIS_PROVIDER`が未設定または`openai`なら、ChatGPTサインインまたはOpenAI APIキー入力を選ぶ画面を出す。入力したAPIキーは`~/.polaris/api_key.json`へ0600で保存する。`codex`を明示した場合とone-shot実行では、この選択画面は出ない。

起動時は常に空の会話から始まる。送信済み会話は`~/.polaris/sessions/<会話id>.jsonl`と`<会話id>.meta.json`に保存し、`/resume`で選んで再開する。起動だけで終了した空の会話は保存しない。`Ctrl-C`で終了する。ツール呼び出しとその中間結果は個別に永続化せず、最終的なアシスタント応答を保存する。

ヘッダーにはモデル、プロバイダ、作業ディレクトリ、累積使用量を表示する。ツール呼び出しは`⚙ ツール名(引数)`、結果は`→ 要約`として表示し、長い表示だけを約200文字で打ち切る。Markdownの太字、インラインコード、コードブロックを整形する。実行中は経過時間と中断キーを表示し、`esc`は待機を中断するが、すでに起動したOSサブプロセスを強制終了しない。

失敗ターンの使用量は累計表示へ反映されない。プロバイダがusageを返さないときは`0`と表示する。履歴ペインはスクロールバックを持たず、最新メッセージだけを表示する。

`/model`はモデル、続けてreasoning effortを選ぶ2段階のピッカーを表示する。表示上の`extra high`はAPIへ送るとき`xhigh`へ変換し、`max`と`ultra`も現在の実装では`xhigh`として送る。明示した選択は既定の`gpt-6-astra`・`medium`を上書きし、次のターンから反映する。`/permissions`と`/skills`も同じ形式のピッカーを使う。`/skills`は閲覧専用である。

`/resume`の一覧は、現在の作業ディレクトリを最初に置き、ほかのディレクトリは最後の会話日時の新しい順に並べる。各行には相対時刻、メッセージ数、最初のユーザーメッセージの60文字プレビューを表示する。30日を超えた会話は絶対日時で表示する。壊れた行がある会話は、その行より前のメッセージまでを再開する。

### スラッシュコマンド

`/`で候補を絞り込み、`↑`/`↓`とEnterで実行する。`/compact`と`/review`を除き、コマンドはモデルへ送らず会話記録にも残らない。

| コマンド | 内容 |
| --- | --- |
| `/help`、`/status`、`/pwd` | ヘルプ、現在の状態、作業ディレクトリを表示する。 |
| `/compact` | 古い履歴を要約へ置換する。閾値超過時は自動実行もする。 |
| `/skills` | 検出したskillを一覧表示する。 |
| `/new`、`/resume`、`/fork`、`/clear` | 会話を新規作成、再開、複製、消去する。`/clear`は元に戻せない。 |
| `/init` | カレントディレクトリに`AGENTS.md`の雛形を作成する。既存時は何もしない。 |
| `/model`、`/permissions` | モデル・推論強度、承認ポリシーを選び直す。 |
| `/diff`、`/review` | 変更点を表示し、またはレビュー指示を通常のモデルターンとして送る。 |
| `/export` | 会話をMarkdownへ書き出す。ファイル名省略時はタイムスタンプ付きの名前を使う。 |
| `/logout`、`/quit`、`/exit` | 保存済み認証を削除し、または終了する。 |

`/resume`は作業ディレクトリ別に会話をグループ化し、現在のディレクトリを先頭に表示する。選んだ会話への送信は既存ファイルへ追記する。MCP、IDE連携、複数セッションのライフサイクル管理など、対応する基盤を持たないCodex由来のコマンドは実装していない。

具体的には、`/mcp`、`/apps`、`/plugins`、`/vim`、`/keymap`、`/hooks`、`/import`、`/memories`、`/theme`、`/pets`、`/ide`、`/plan`、`/goal`、`/agents`、`/subagents`、`/side`、`/btw`、`/ps`、`/stop`、`/title`、`/statusline`、`/feedback`、`/personality`、`/experimental`、`/approve`、`/mention`、`/copy`、`/rename`、`/archive`、`/delete`、`/cd`は利用できない。`/usage`は`/status`のトークン表示と重なるため設けない。

## subagent

`spawn`の型、設定、出力検証、深さの制限は[subagent](subagents.md)にある。

## 隔離実測のbroker連携

外側のnative sandboxからツールを実行する開発用連携では、launcherが`POLARIS_SANDBOX_BROKER`と`POLARIS_SANDBOX_BROKER_TOKEN`を組で設定する。未指定なら通常の実行経路を使う。接続先は同じlauncherが渡す`HOME`以下の`.local/share/codex-benchmarks/<32桁の小文字16進ID>/broker.sock`に限定し、非絶対パス・親ディレクトリ参照・リンク経由の接続を拒否する。任意の接続先rootを許可する設定はない。

`HOME`・socket・tokenは信頼するlauncherの設定として扱う。実際の接続先、ファイル範囲、ツールの実行権限は外側のsandboxとbrokerが独立して制限する。公開パッケージに共通実測ランナー自体は同梱しない。
