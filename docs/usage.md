# 利用ガイド

## ビルドと一発実行

Rust 1.96.0とedition 2024を使う。

```sh
cargo build --release
```

リリースビルド前は`cargo run -p polaris-cli --`の後ろに同じ引数を続けられる。`exec [PROMPT]`は`--prompt`/`-p`と同じone-shot実行の別名であり、引数を省略するか`-`を渡すと標準入力から指示を読む。

| 変数 | 既定値 | 意味 |
| --- | --- | --- |
| `POLARIS_PROVIDER` | `openai` | `openai`または`codex`。`codex`は`polaris login`で得たChatGPT認証を使う。 |
| `POLARIS_API_KEY` | なし | `POLARIS_PROVIDER=openai`で必要なOpenAI互換エンドポイントのAPIキー。 |
| `POLARIS_BASE_URL` | `https://api.openai.com/v1` | `openai`プロバイダのベースURL。 |
| `POLARIS_MODEL` | `gpt-6-astra` | 両プロバイダ共通の既定モデル。明示した`--model`が優先する。 |

`POLARIS_CACHE_PACING`の既定は`off`。`on`は同じCodex providerのモデル要求開始を最低5秒間隔にする実験設定で、完了までの時間が伸びる場合がある。[比較条件と結果](gpt6-cache-pacing-results.md)を参照。

`--web-search`は`disabled`、`cached`、`live`を受け付けるが、現在のChatGPT認証先はResponses形式の上限契約を確認していない。既定の`disabled`だけが利用でき、`cached`と`live`は明示しても開始前に拒否される。`cached`もローカルのオフライン検索を意味しない。動作するlive用コマンドはまだ案内しない。

推論強度の既定は`medium`。APIにはモデル名を`gpt-6-astra`、推論強度を`medium`として別々に渡す。`--effort`で明示した値が優先され、アカウントのプランによって既定値は変わらない。

現行の`openai`プロバイダは`/chat/completions`を使う。公式仕様ではAstraのtool callingにResponses APIが必要なため、この経路でのAstraのツール実行は非対応である。`codex`プロバイダは別のResponses形式の実装であり、APIキー用のResponses対応とは区別する。[Astra移行ガイド](https://developers.openai.com/api/docs/guides/latest-model)

`polaris sandbox -- <コマンド...>`はエージェントループを介さず、選択中のサンドボックスポリシーでコマンドを実行し、終了コードと標準出力・標準エラーを返す。`polaris doctor`は保存済み資格情報、既定プロバイダ、サンドボックスの可否を読み取り専用で診断し、鍵の中身は表示しない。`polaris completion <bash|zsh|fish|elvish|powershell>`は補完スクリプトを標準出力へ生成する。

## workflowと厳密履歴

設定は`~/.polaris/config.toml`と`<project-root>/.polaris/config.toml`に置く。プロジェクトで明示した値が優先し、未指定の値はグローバル設定を継承する。

workflowはv0.11.0から既定で有効である。常時skillと段階別skillを明示的に解決して各ターンへ固定し、通常のskill検索ランキングとは混ぜない。段階は`general`、`brainstorm`、`specify`、`implement`、`review`、`verify`、`deliver`である。

```toml
[workflow]
enabled = true
initial_phase = "specify"
always = ["user:team-rules"]

[workflow.phase_skills]
implement = ["builtin:implement"]
review = ["user:review-rules"]
```

比較などで無効にする場合は`[workflow] enabled = false`を明示する。`--phase <段階>`はworkflowが有効な起動だけで使える。`--resume <v2 UUID>`は保存済みv2会話を続け、`--fork <v2 UUID>`は指定会話から新しいUUIDを作る。両方は同時に指定できない。旧JSONLのIDを`--resume`へ渡した場合は、新しいv2 UUIDへ一度だけ取り込む。workflow状態を含む保存済み会話を、workflowを無効にして再開することはできない。

履歴方式の既定は`legacy`である。`--history-mode strict10`は直近10個の実ユーザーターンを保持し、固定したローカル埋め込み設定を必要とする。設定方法と保存境界は[コンテキスト効率化](context-efficiency.md)を参照。

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

通常のlegacy起動は空の会話から始まり、送信済み会話を`~/.polaris/sessions/<会話id>.jsonl`と`<会話id>.meta.json`へ保存する。workflowまたはstrict10を有効にした会話は`~/.polaris/sessions-v2/<UUID>/`を使う。v2はrawイベント、世代付きsnapshot、状態markerを保存するため、ツール呼び出しと中間結果も回復対象になる。起動だけで終了したlegacyの空会話は保存しない。`Ctrl-C`で終了する。

ヘッダーにはモデル、プロバイダ、作業ディレクトリ、累積使用量を表示する。ツール呼び出しは`⚙ ツール名(引数)`、結果は`→ 要約`として表示し、長い表示だけを約200文字で打ち切る。Markdownの太字、インラインコード、コードブロックを整形する。実行中は経過時間と中断キーを表示し、`esc`は待機を中断するが、すでに起動したOSサブプロセスを強制終了しない。

失敗ターンの使用量は累計表示へ反映されない。プロバイダがusageを返さないときは`0`と表示する。履歴ペインはスクロールバックを持たず、最新メッセージだけを表示する。

`/model`はモデル、続けてreasoning effortを選ぶ2段階のピッカーを表示する。表示上の`extra high`はAPIへ送るとき`xhigh`へ変換し、`max`と`ultra`も現在の実装では`xhigh`として送る。明示した選択は既定の`gpt-6-astra`・`medium`を上書きし、次のターンから反映する。`/permissions`と`/skills`も同じ形式のピッカーを使う。`/skills`は閲覧専用である。

`/resume`の一覧は、現在の作業ディレクトリを最初に置き、ほかのディレクトリは最後の会話日時の新しい順に並べる。各行には相対時刻、メッセージ数、最初のユーザーメッセージの60文字プレビューを表示する。30日を超えた会話は絶対日時で表示する。legacyでは壊れた行より前までを読むが、v2の破損・世代不一致は原文を切り捨てず拒否する。

### スラッシュコマンド

`/`で候補を絞り込み、`↑`/`↓`とEnterで実行する。`/compact`と`/review`を除き、コマンドはモデルへ送らず会話記録にも残らない。

| コマンド | 内容 |
| --- | --- |
| `/help`、`/status`、`/pwd` | ヘルプ、現在の状態、作業ディレクトリを表示する。 |
| `/compact` | 古い履歴を要約へ置換する。閾値超過時は自動実行もする。 |
| `/phase` | workflowが有効なら現在の段階を表示し、`/phase implement`のように次ターンから使う段階を保存する。 |
| `/skills` | 検出したskillを一覧表示する。 |
| `/new`、`/resume`、`/fork`、`/clear` | 会話を新規作成、再開、複製、消去する。`/clear`は元に戻せない。 |
| `/init` | カレントディレクトリに`AGENTS.md`の雛形を作成する。既存時は何もしない。 |
| `/model`、`/permissions` | モデル・推論強度、承認ポリシーを選び直す。 |
| `/diff`、`/review` | 変更点を表示し、またはレビュー指示を通常のモデルターンとして送る。 |
| `/export` | 会話をMarkdownへ書き出す。ファイル名省略時はタイムスタンプ付きの名前を使う。 |
| `/logout`、`/quit`、`/exit` | 保存済み認証を削除し、または終了する。 |

`/resume`は作業ディレクトリ別に会話をグループ化し、現在のディレクトリを先頭に表示する。選んだ会話への送信は既存ファイルへ追記する。MCP、IDE連携、複数セッションのライフサイクル管理など、対応する基盤を持たないCodex由来のコマンドは実装していない。

### files.mdの自動更新

files-md-writer型を導入している場合、rootのツール変更後に対象ディレクトリの`files.md`を自動更新する。設定の既定は有効であり、不要なプロジェクトでは、グローバルまたはプロジェクト設定で止められる。

```toml
[files_md]
auto_regenerate = false
```

この設定はroot起点の自動更新だけを止める。`spawn`による明示的なsubagent実行は維持される。

具体的には、`/mcp`、`/apps`、`/plugins`、`/vim`、`/keymap`、`/hooks`、`/import`、`/memories`、`/theme`、`/pets`、`/ide`、`/plan`、`/goal`、`/agents`、`/subagents`、`/side`、`/btw`、`/ps`、`/stop`、`/title`、`/statusline`、`/feedback`、`/personality`、`/experimental`、`/approve`、`/mention`、`/copy`、`/rename`、`/archive`、`/delete`、`/cd`は利用できない。`/usage`は`/status`のトークン表示と重なるため設けない。

## subagent

`spawn`の型、設定、出力検証、深さの制限は[subagent](subagents.md)にある。

## 隔離実測のbroker連携

外側のnative sandboxからツールを実行する開発用連携では、launcherが`POLARIS_SANDBOX_BROKER`と`POLARIS_SANDBOX_BROKER_TOKEN`を組で設定する。未指定なら通常の実行経路を使う。接続先は同じlauncherが渡す`HOME`以下の`.local/share/codex-benchmarks/<32桁の小文字16進ID>/broker.sock`に限定し、非絶対パス・親ディレクトリ参照・リンク経由の接続を拒否する。任意の接続先rootを許可する設定はない。

`HOME`・socket・tokenは信頼するlauncherの設定として扱う。実際の接続先、ファイル範囲、ツールの実行権限は外側のsandboxとbrokerが独立して制限する。公開パッケージに共通実測ランナー自体は同梱しない。
