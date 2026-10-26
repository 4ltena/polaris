# コマンドの役割と出力

2026-10-26時点のソースを基にした一覧。生成するhelp・ログ・エラーは英語で表示する。主な対象はmacOSの対話TUIとCLIである。出力例の`<ID>`・`<path>`・数値は説明用で、実際の実行結果ではない。表中の日本語は表示内容の説明であり、生成する実際の文言は英語である。設定や保存状態によって、確認画面やエラーが先に出る場合がある。

- [TUIコマンド一覧](#tuiコマンド一覧)
- [シェルで使うCLI](#シェルで使うcli)
- [モデル内部のツール](#モデルが呼ぶツールと画面の読み方)
- [現在の提供状況](#現在の提供状況で注意する点)

## 入力する場所

| 種類 | 入力例 | 動作 |
| --- | --- | --- |
| シェルで実行するCLI | `polaris doctor` | Polarisの起動や、単独の管理操作を行う。 |
| Polaris内のTUIコマンド | `/status` | 開いている会話や実行の状態を表示・変更する。 |
| モデルに送る文章 | `このプログラムの構成を説明して` | モデルが応答し、必要に応じてツールを呼ぶ。 |
| モデル内部のツール | 画面に出る`read(...)`や`skill(...)` | モデルが選ぶ操作。スラッシュコマンドとして直接入力するものではない。 |

TUIでは`/`で候補を出し、入力で絞り込み、↑↓とEnterで選ぶ。引数が必要な操作は、入力欄にコマンド全文を書く。例えば`/subagents cancel <child-id> <attempt-id>`では、`<...>`を一覧に出た実際のIDへ置き換える。

実行中（working）も補完を使える。`/input`・`/instruction`・`/subagents`・`/run details`は実行中に処理される。ほかのコマンドは原則として待ち行列へ入り、実行終了後に処理される。秘密入力の操作は実行中には受け付けない。

## TUIコマンド一覧

### 環境構築（macOS arm64）

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `/env set <profile> [--version <constraint>] [--path <directory>]` | 1回の計画承認から固定資源の取得、private runtime、offline導入、検証まで進める。モデル呼出なし。 | 対象・版・資源数/bytes・SHA-256・作成先・保存先の確認。完了時に`Environment verified`とreceipt。 |
| `env set <profile> ...` | 入力全文が同じ構文に一致する場合だけ、上記の直接操作として扱う。 | 上と同じ。自然文や引用中の部分一致は実行しない。 |
| `/env plan <profile> ...` | 対象を読み取り、固定計画をprivate領域へ保存する。取得・導入はしない。 | 計画の概要、SHA-256、plan/definitionの保存先。 |
| `/env status` | 固定profileと対応版を列挙する。 | `available`と、実行ごとのreceiptで検証を区別する案内。 |
| `/env status --operation <retained-root>` | 保存済み操作を読み取り照合する。 | stage・outcome・一致/競合・未完了のJSON。 |
| `/env recover --operation <retained-root>` | 検証済みreceiptに基づき、欠けた最終管理記録だけを補う。 | 照合結果。通信・process・publishは繰り返さない。 |

profileは`node`・`nextjs`・`react`・`vue`・`svelte`・`astro`・`nuxt`・`angular`・`python`・`rust`・`go`。対応外の版・OS・CPUは拒否する。相対pathと省略時の対象は起動時cwdを基準にする。既存manifest/lock/sourceは保持し、既存の依存treeを削除・上書きしない。安全に対応できる固定依存に限定し、更新や再生成が必要なら停止する。

実行中はEscまたはCtrl-Cで停止を要求できる。停止・失敗時のprivate資源と部分反映は保持し、未検証を成功と表示しない。常駐dev serverやglobal設定の変更は行わない。

### 状態・ヘルプ・表示

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `/help`、`/` | 利用できるコマンドを調べる。 | `commands:`に続くコマンド名と説明の一覧。 |
| `/status` | 現在の設定、利用条件、使用量、workflow、管理状態を確認する。モデルへの送信や接続確認は行わない。 | アカウント・利用条件・usage/workflow・可用性・管理診断を項目ごとに表示。対話ID、保存済みログイン状態・使用登録、provider/model/effort、承認・sandbox、tokens、phase/skills、未確認事項、audit、CAT等。 |
| `/doctor` | ローカルの認証設定の有無、保存先、sandbox、指示ファイル、復旧状態を診断する。 | 認証ファイル・環境キーの存在、provider/model、状態保存先、AGENTS到達量、MCP・CAT・巻き戻し・管理診断。認証値は表示しない。 |
| `/pwd` | 現在のproject rootを確認する。 | 作業ディレクトリの絶対パス。 |
| `/diff` | 未コミットの変更を見る。 | `diff:`に続く未stage・stage済みの差分。未追跡ファイルは`?? <path>`で名前だけを表示。最大200行、変更なしなら`(no changes)`。 |
| `/skills` | 検出されたskillを閲覧する。 | 名前・説明・探索元を一覧表示。↑↓で選び、Enter/Escで閉じる。選択による有効化は行わない。 |

`/status`のtokensは観測済みの使用量であり、アカウントの残り利用枠ではない。認証・モデル・MCPの接続や、capacityの空きを確認済みという意味でもない。

### モデル・承認・workflow

| コマンド | 役割 | 主な出力・反映時期 |
| --- | --- | --- |
| `/model` | 次のターンで使うモデルを選ぶ。 | `gpt-6-astra`、`gpt-6.1-sol`、`gpt-6-luna`、`Local model`の選択画面。成功時`model set to <model>`。 |
| `/permissions` | 起動時に許された範囲で承認方針を選ぶ。 | Never / OnRequest / Alwaysの選択画面と`permissions set to <policy>`。起動時の境界を超える選択は拒否し、再起動用の引数を案内する。 |
| `/sandbox [status]` | 起動時に固定されたsandboxと承認方針を確認する。 | `sandbox: ...`、`approval: OnRequest`等。sandboxの変更は新しい起動に適用する。 |

`/model`の`Local model`はloopbackのOllama/LM Studioへモデル在庫を問い合わせる。ツール対応を確認できたモデルを選び、成功時に`local model set to Ollama · <model>`等を表示する。候補がなければ`no connected local models`。推論強度を選ぶ画面ではない。

| workflowコマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `/phase [段階]` | 現在の段階を表示し、指定時は確認後に次ターンの段階を保存する。 | `ワークフロー段階: <段階>`。値はgeneral/brainstorm/specify/implement/review/verify/deliver。 |
| `/workflow` | 現在の実行方式を見る。 | `workflow: manual/adaptive/off`。 |
| `/workflow [mode] manual\|adaptive\|off` | 確認後、実行方式を保存する。 | 保存後のmode。`mode`という語は省略できる。 |

### 会話の作成・再開・保存

| コマンド | 役割 | 主な出力・操作 |
| --- | --- | --- |
| `/new` | 元会話を残し、空の会話を始める。 | 会話画面をresetして待機状態へ戻る。新しいIDは`/session`で確認できる。 |
| `/resume` | 保存会話を選んで開く。 | `resume a conversation`と、経過時間・message数・previewの一覧。↑↓で移動、Enterで選択、Escで取消。候補1件なら選択位置は変わらない。 |
| `/fork` | 現在の履歴を別の会話IDへ複製する。 | `v2セッションへ分岐しました: <ID>`。 |
| `/session`、`/session current` | 会話IDと現在・保存済みの設定を確認する。 | `会話: <ID> / 世代 <n>`、`現在provider: ... / model: ...`、保存provider/model/effort、履歴・remember・tool-memory・compact等。 |
| `/history [offset]` | 保存した原文履歴を読む。offsetはmessage単位の整数。 | `原文履歴 <開始>..<終了> / <総数> messages`とrole/turn/本文。1回最大20messages・64KiB。 |
| `/clear` | 確認後、会話の現在の区間（epoch）を消去する。 | `T08 complete`。過去の証拠は保持されるため、全履歴ファイルの削除とは異なる。 |
| `/compact` | 確認後、モデルに古い会話を要約させる。原文は保持する。 | `compacting...`→`compacted <前> messages → <後> (<前tok> tok → <後tok> tok)`。短い会話なら`nothing old enough to compact yet`。 |
| `/review [追加指示]` | 現在の未コミット変更のレビューをモデルへ依頼する。 | 通常のモデル応答とツールの実行状況。追加指示をレビューpromptに含める。 |
| `/export [path]` | 確認後、会話原文のeventをMarkdownへ新規保存する。 | `T17 完了: <path>`。省略時は`polaris-export-<時刻>.md`。既存ファイルは上書きせず、上限16MiB。 |
| `/copy` | 確認後、最後の本文があるassistant返答をclipboardへコピーする。 | `T19 完了`。 |
| `/init` | 確認後、project rootに`AGENTS.md`の雛形を新規作成する。 | `T10 完了: <path>`。既存ファイルは上書きしない。 |
| `/quit`、`/exit`、`/q` | TUIを終了する。 | 保存会話があれば`Resume this session with: polaris --resume <ID>`。未処理入力や下書きがあれば終了を拒否する場合がある。 |

`/resume`で会話を開いても、現在の作業ファイルと保存状態が一致しない場合は閲覧のみになる。`/new`・`/fork`・`/resume`は別の操作であり、IDは`/session current`で確認できる。

ログイン画面・ダイアログ・実行中も、可視文字をドラッグして離すと選択範囲を直接clipboardへコピーする。非表示の秘密値は対象外で、追加の確認画面は表示しない。リンクはCtrl-click、端末が通知する場合はCmd-clickで開く。通常のクリック・ドラッグでは開かない。

macOSでは、会話に表示された実在するファイル名やローカルパスを宛先とするMarkdownリンクをCtrl-click（端末対応時はCmd-click）すると、Finderでそのファイルの所在を表示する。ファイル自体は開かない。相対パスはproject rootから解決し、`src/main.rs:12:3`のような行・列指定にも対応する。日本語・折り返されたパス、インラインコードで示された空白入りのファイル名も対象になる。ドラッグ操作は範囲選択として扱う。Webリンクや存在しないファイルはFinderで開かない。

### ファイルのD&Dと入力リンク

macOSの端末へファイルをD&Dすると、入力欄に`[ファイル名]`のリンクブロックを挿入する。複数ファイルはそれぞれ別のブロックになる。ファイル本文は自動では読み込まず、Enterまでモデルへ送信しない。

左右キーはブロックをまたいで移動し、直後でBackspaceを押すとブロック全体を削除する。下書きの復元や入力履歴の呼び出し後も同じように編集できる。通常の文章は文字単位で編集する。

端末がD&Dをパス文字列として渡す場合は、入力が途切れた後に実在する絶対パスをリンク化する。同じ形で手動入力したパスも対象になる。存在しないファイルやフォルダ、Web URLはファイルブロックへ変換しない。入力・下書きの既存上限を超える貼り付けは、途中まで挿入せず拒否する。

### 会話設定・goal

設定の変更は確認後に保存される。成功表示は`F06 を保存しました（会話世代 <n>）。`のような操作種別と世代で、取消時は`Session settings change cancelled`。

| コマンド | 役割 | 主な出力・条件 |
| --- | --- | --- |
| `/session configure <provider> <model> [effort]` | 現在と同じproviderで、保存会話のモデル設定を変更する。 | 設定の確認とD18の保存結果。異なるproviderの指定は拒否し、再起動を案内する。新しい固定effortへの変更も拒否する。modelは確認済みのものに限る。 |
| `/session history` | 履歴方式を見る。 | 現在のlegacy/strict10/structured10。 |
| `/session history legacy\|strict10\|structured10` | 空会話の履歴方式を変更する。 | F16の保存結果。既存履歴のある会話は変更不可。 |
| `/memory configure on\|off` | 圧縮前履歴の記憶設定を変更する。 | F06の保存結果。 |
| `/memory configure legacy\|strict10\|structured10` | 履歴方式の変更別名。 | `/session history <方式>`と同じ。 |
| `/tool-memory off\|history\|retrieval` | ツール結果の保存・参照方式を変更する。 | F07の保存結果。 |
| `/tool-memory endpoint off`、`/tool-memory endpoint <URL> <MODEL>` | ツール記憶の埋め込み接続を無効化、または接続先とモデルを保存する。 | F08の保存結果。 |
| `/tool-memory model <MODEL>` | 設定済みの埋め込みモデルを変更する。 | F09の保存結果。先にendpoint設定が必要。 |
| `/compact threshold off\|<整数>` | 自動圧縮の閾値を変更する。 | F10の保存結果。整数は1000以上。 |
| `/goal`、`/goal current` | 保存したgoalを見る。 | `goal revision <n> / 有効 on / 停止 off`と本文。未設定なら`No saved goal`。 |
| `/goal set <文章>`、`/goal clear` | goalを設定、または消去する。 | 確認後、D20の保存結果とgoal表示。 |
| `/goal stop`、`/goal resume` | 保存goalを停止、または再開する。 | 確認後、D20の保存結果と停止状態。 |

goalの保存本文や過去の記録は、それ自体では実行権限や承認にならない。

### 入力待ち行列・実行・subagents

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `/input status <受理ID>` | 送信・待機中の入力を正確なIDで調べる。 | `入力 <ID> / revision <n> / 状態 <状態>`。 |
| `/input cancel <受理ID>` | 確認後、待機入力を取り消すか、一致する実行に停止要求を出す。 | `入力 <ID> を取り消しました`等。実行中なら対象runとchild/MCPの終了確認を行う。 |
| `/instruction send <文章>` | 次のrunへ適用する指示を保存する。実行中にも受け付ける。 | `指示受理ID: <ID>。次のrun境界で適用します`。 |
| `/subagents`、`/subagents list` | 現在runの子を一覧表示する。 | `run=<ID> attempt=<ID> revision=<n>`、続いて`<child-ID> / <attempt-ID>: running role=... tier=... model=... effort=...`。 |
| `/subagents wait <child-ID> <attempt-ID>` | 指定attemptの状態を最大2秒の範囲で取得する。完了まで待ち続ける操作ではない。 | `revision=<n> cursor=<n>`と対象の状態行。停止後などは`outcome_unknown`の場合がある。 |
| `/subagents cancel <child-ID> <attempt-ID>` | 対象を確認後、そのattemptへ停止要求を出す。 | `停止要求 revision=<n>`、`Stop request delivered to this attempt. Check its terminal state with /subagents wait`。 |
| `/roles [list]` | 現在のrole定義を表示する。 | `<名前>: tier=<最小>..<最大> access=... turns=<n> wall=<n>s`等。bindingは変更しない。 |
| `/run`、`/run settings` | 次のrunのturn上限を見る。 | `次のrun: max-turns=<n>（起動時上限=<n>）`。 |
| `/run settings max-turns <n>` | 次のrunの上限を変更する。 | 上と同じ。指定は1以上、起動時上限以下。 |
| `/run details` | 現在runの子とツール観測を詳しく見る。 | subagents一覧とツールの詳細。 |

child-IDは一覧の`/`の左、attempt-IDは右から`:`の直前まで。例えば、次の行ならその下のコマンドで照会する。

```text
child.example.1 / child.example.1.attempt.1: running role=file-reader ...

/subagents wait child.example.1 child.example.1.attempt.1
```

親runのownerが終了した後は、そのrunのwait/cancelは使えない。ownerがなければ`The current run has no subagent owner`と表示する。

### 認証・アカウント

| コマンド | 役割 | 主な出力・反映時期 |
| --- | --- | --- |
| `/status` | 保存されたログイン状態、現在の使用登録、対話ID、使用トークンを確認する。 | 登録ID・表示名・状態、使用登録の世代、対話ID、入力・出力・cache・総tokens。認証更新や接続確認は行わない。 |
| `/login`、`/login new [表示名]` | ブラウザ認可でアカウントを登録する。 | 認可の案内と登録完了・登録ID。登録だけでは現在の使用登録を変更しない。 |
| `/login reauth <ID>` | 指定登録を再認証する。 | ブラウザ認可と再認証完了・登録ID。 |
| `/login use <ID>` | 次の送信から使う登録を切り替える。 | `使用アカウントを切り替えました: <ID>`。 |
| `/login label <ID> <表示名>` | 登録の表示名を変更する。 | 変更完了・登録ID。 |
| `/logout` | 使用中の登録の認証を解除するか、旧単一認証を削除する。 | 追加確認なしで実行し、解除・削除の結果。 |
| `/logout <ID>` | 指定登録の認証を解除する。 | 追加確認なしで実行し、解除完了・登録ID。 |

TUIの`/account`は廃止し、上記の3入口へ集約した。CLIの`polaris account`は保持している。CLIの`polaris login`は単一認証、TUIの`/login`はアカウント登録であり、処理が同一ではない。`/status`のログイン状態は保存情報であり、サーバーへの認証成功を再検証した結果ではない。

### MCP

| コマンド | 役割 | 主な出力・動作 |
| --- | --- | --- |
| `/mcp`、`/mcp list`、`/mcp status` | 登録とホストの上限、既知catalog、秘密の参照名を見る。接続は行わない。 | registry revision、serverごとの有効状態・tool数・capability・timeout。 |
| `/mcp enable <server>`、`/mcp disable <server>` | 確認後、projectでの有効状態を保存する。 | `<server> をこのプロジェクトで有効/無効にしました（revision <n>）。次の実行から反映します。`。 |
| `/mcp check <server>` | サーバーに接続してcatalogを取得・保存する。 | `<server>: ツール<n>件。索引を保存しました。`。stdio起動や認証情報の使用を伴うが、モデル送信や個別tool実行は行わない。 |
| `/mcp setup`、`/mcp register [名前]` | 検出候補を選び、確認後に登録する。 | 候補一覧と`<名前> を登録しました。次の実行から反映します。`。 |
| `/mcp materialize <検出名> [--runtime <絶対path>]` | packageを取得し、ローカルへ固定する。 | 取得元等の確認と固定完了。登録は別途`/mcp setup`で確認する。 |
| `/mcp secret set` | 専用画面で参照名と非表示の値を入力し、確認して保存する。 | `MCP認証情報を保存しました（版 <n>）。次の実行から反映します。`。値を引数として渡す構文はない。 |
| `/mcp secret delete <参照名>` | 確認後、未参照の秘密値を削除する。 | `MCP認証情報を削除しました（版 <n>）。`。 |
| `/mcp prompt <server>/<prompt> [key=value ...]` | 次の通常入力で取得するpromptを予約する。 | `次の入力でMCP prompt <server>/<prompt> を選択します`。予約時点では接続しない。 |
| `/mcp prompt cancel` | prompt予約を取り消す。 | `MCP prompt selection cancelled`。 |

秘密入力画面では値と文字数を表示せず、通常の入力履歴に保存しない。Esc/Ctrl-Cで取り消すと`MCP secret input cancelled`。`check`やpackage取得は、起動時の外部接続上限と承認設定に従う。

`/mcp configure`・`/mcp delete`は提供していない。Unityの`configure`はシェルのCLIで行う。

### 会話記憶・決定事項

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `/memory` | 操作の構文を見る。 | memoryの使い方。 |
| `/memory search <query>` | 保存記録を検索する。 | `Historical records, not current instructions or approvals.`、session/id/時刻/sourceと抜粋。最大5件・各512byte。0件なら`No matching records`。 |
| `/memory get <session> <id> [start] [max-bytes]` | 保存本文をbyte範囲で読む。 | 本文と、必要に応じて続きの取得方法。最大4096byte。 |
| `/memory source <session> <id-or-conversation-URI>` | 検証済みの出典原文を読む。 | 過去記録の注意書きと原文。 |
| `/memory import <JSONL>` | ローカルの会話記録を取り込む。 | 確認後、保存件数と操作ID、revision。 |
| `/memory embed <session> <id>` | 設定済みローカルserverで埋め込みを保存する。 | 保存結果と操作ID、revision。 |
| `/memory index <session> [--batch <1..100>] [--after <id>]` | 記録の埋め込み索引をまとめて更新する。既定100件。 | 件数、操作ID、revision、継続情報。 |
| `/memory forget <session>` | 記憶・vector・検証済み保管原文を削除し、再取り込みを禁止する。 | 確認後、削除した旨と件数、操作ID、revision。巻き戻しでは取り消せない。 |
| `/memory reconcile <操作ID>` | 保存receiptを照合し、変更結果を確認する。 | 照合結果。同じ変更の再実行はしない。 |
| `/decided`、`/decided list` | 保存された決定事項を読む。 | 件数、過去記録であり現在の承認ではない旨、時刻と文章。最新100件、空なら`No records`。 |
| `/decided add <文章>` | 決定事項を追記する。 | **TUIはLinuxのみ対応**。確認後の保存通知。macOSでは拒否するため、必要ならCLIの`polaris decided add`を使う。 |

`/memory configure ...`は会話設定の節を参照。記憶の変更では`記録の変更を保存しました。記録<n>件、v2索引削除<n>件。`等と`操作ID: <ID> / revision <前> → <後>`を表示する。埋め込み処理は設定済みloopback APIへ接続する。

### フォルダ比較・共有同期

| コマンド | 役割 | 主な出力・現在の制限 |
| --- | --- | --- |
| `/compare <folder1> \| <folder2> [\| ...]` | 2〜8フォルダを名前とサイズで比較する。 | `<path1> / <path2>: 類似度 <値>、共通 <n>件`と提案。本文の読取・移動・削除は行わない。 |
| `/sync`、`/sync status` | 共有の参加・合意・反映・衝突・復旧状態を見る。 | 参加中/未参加、revision、解消待ち、未完了操作、復旧用控え等。 |
| `/sync join <remote> <share-ID> <invite-ID> <expires-UNIX秒> [除外名...]` | TUIから共有への参加を要求する入口。 | **現状のTUI経路は未完成で、参加を確定できない**。結果を確定できない旨と`/sync`での確認案内。CLIのC08経路とは別。 |
| `/sync pull` | 既存の参加情報を使いremoteから受信・統合する。 | `受け取りました: <n>件、解消待ち<m>件。sourceへは未反映です。`。 |
| `/sync leave` | この端末の参加を解除する。 | `This device left the share`。 |
| `/sync apply` | 確認後、同期内容をsourceへ書き込む。 | `<n>件を反映しました。復旧用の控え: <path>`。 |
| `/sync recover <64桁hex-key>` | source適用の保存証拠を照合する。 | 管理操作の照合結果。sourceの再適用はしない。 |
| `/sync push` | 共有内容を送信する。macOS Keychainが前提。 | `C10送信完了: operation=<ID> commit=<hash>`、または共有内容が一致する旨。 |
| `/sync recover <32桁C10-ID>` | 送信結果を照合・復旧する。remote照会を伴う場合がある。 | C10結果。結果未確定なら再送せず照合する案内。 |
| `/sync abandon <32桁C10-ID>` | 確認後、C10操作を照合して中止する。 | 中止通知と`does not prove that nothing was sent`。 |

### Linux限定のTUI操作

`/setting`はmacOSでも候補に出るが、実行handlerは接続されていない。macOSでは`This operation requires an active terminal and storage owner`と表示する。CLIに同名の機能がある場合も、TUI側の対応とは別である。

| コマンド | Linuxでの役割 | 主な出力 |
| --- | --- | --- |
| `/setting git` | `.gitignore`追加規則を確認し、保存する。 | 候補の確認と`Already added: <rule>`。 |

`/apply`はLinuxだけの候補で、macOSでは候補にも出ない。

| コマンド | Linuxでの役割 | 主な出力 |
| --- | --- | --- |
| `/apply`、`/apply list`、`/apply status` | 隔離runの変更候補を見る。 | 承認ID、entries件数、decision、result。候補なしならrunの案内。 |
| `/apply run <指示>` | 確認後、隔離コピーでモデルを実行する。 | runの終端状態と、`/apply list`で候補を確認する案内。 |
| `/apply view <ID>` | 候補の適用先・復旧先・revision・hash・mode等を見る。 | pathごとのbefore/after情報。本文diffではない。 |
| `/apply resolve <ID> allow\|deny` | 確認後、適用への承認または拒否を返す。 | 承認応答の受付と、保存済み結果を`/apply list`で確認する案内。受付だけでは適用成功を意味しない。 |

## シェルで使うCLI

### 環境構築

Rustの既存Command Line Toolsを使う場合は、起動時に`POLARIS_ENVKIT_APPLE_TOOLCHAIN=command-line-tools`を指定する。未指定時は現在のApple developer選択を維持する。詳細は[利用方法](usage.md)を参照。

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `polaris env set <profile> [--path <directory>] [--version <constraint>]` | TUIと同じownerで環境構築する。モデル認証は不要。 | 計画、`Execute this exact plan? [y/N]`、検証結果とreceipt/private runtimeの場所。 |
| `polaris env plan <profile> ...` | 取得前の固定計画を確認・保存する。 | 対応版、資源、変更先、SHA-256と保存先。対象の構築はしない。 |
| `polaris env set <profile> ... --plan-directory <retained-root> --approve-plan <SHA-256>` | まだ開始していない保存計画を、同じ対象・版・SHA-256への明示承認で実行する。 | 通常setと同じ。入力変更や開始済み計画は拒否する。 |
| `polaris env status` | 定義と版を列挙する。 | 各profileの`available`と、組合せごとの検証状態についての案内。 |
| `polaris env status --operation <retained-root> [--path <project>]` | 保存計画と生成物・runtimeを照合する。 | stage、一致/競合/未完了、receipt hashのJSON。通信・process・publishを繰り返さない。 |
| `polaris env recover --operation <retained-root> [--path <project>]` | 検証済みreceiptの最終管理記録だけが欠けた場合に照合して補う。 | 照合結果。途中のpublishや内容不一致は未完了のまま保持する。 |

`--path`はset/planでは導入対象、status/recoverでは操作を所有するprojectの場所。全profileはmacOS arm64向けである。`available`は固定資源が定義済みであることを示し、取得・導入・動作が今回検証済みという意味ではない。検証結果はprofile・adapter・実行binary・OS・対象・必要なnative toolchainを束縛したlocal receiptで確認する。

### 起動・診断・補完

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `polaris` | 対話TUIを起動する。 | 会話、入力欄、ツールの実行状況。認証がなければ条件に応じて認証方法の選択画面。 |
| `polaris -p "<指示>"` | 指示を1回実行して終了する。 | stdoutへ回答本文。stderrへworkflow・ツール診断、`tokens: in ... / out ... / cache ... / total ...`等。モデル接続・ツール実行・監査保存を伴う。 |
| `polaris exec [<指示>\|-]` | `-p`と同じ非対話実行。省略または`-`ならstdin全文を読む。 | 上と同じ。 |
| `polaris sandbox -- <実行物> [引数...]` | エージェントを介さず、指定プログラムをsandbox内で実行する。 | 子のstdout/stderrと終了コード。失敗時はsandboxエラー。 |
| `polaris doctor` | インストールとローカル設定を診断する。 | `Decision capacity (read-only):`で物理メモリの観測、容量帯の候補順、実行可否が未検証であることを表示。統合メモリは専用VRAMと区別する。続いて`Credentials:`、`Provider resolution:`、`Sandbox:`、`State:`、巻き戻し候補、AGENTS到達量等。接続成功の確認ではない。 |
| `polaris decision list` | package登録済み判断profileと容量の推薦を表示する。 | JSONの`registered_profiles`・`capacity`・注意書き。現在の登録は0件。導入・接続・推論・backend変更なし。 |
| `polaris decision verify <profile>` | 導入済み固定generationを読み取り専用で検証する。 | worker/modelの全size・SHA、受入subjectのrevision・量子化・generationをJSON表示。未受入・未導入・破損・非対応は失敗。copy・有効化・推論なし。読み取り後の状態や実行可否の保証ではない。 |
| `polaris decision plan <profile> --worker <絶対path> --model <絶対path>` | 固定artifactの導入内容を確認する。 | 新しいtransactionの計画JSONと`consent_sha256`。取得元、license/NOTICE、component、size/SHA、導入先。copyなし。未受入・配布情報不足なら理由を表示して終了。 |
| `polaris decision import <profile> --worker <絶対path> --model <絶対path> [--yes]` | 今回の計画を確認後、固定artifactを新generationへcopyする。 | 計画JSON、`y/n`、導入または取消/失敗の通知。非対話は明示`--yes`が必要。network・backend有効化・推論なし。既存generationを上書きしない。 |
| `polaris decision plan <profile> --worker <絶対path> --model <絶対path> --repair` | 既存generationを保持する修復計画を確認する。 | 通常の計画に加え、旧generationの識別情報と`.recovery-*`の保持先を表示。copyなし。 |
| `polaris decision repair <profile> --worker <絶対path> --model <絶対path> [--yes]` | 検証したartifactで修復する。 | 計画JSON、確認、修復結果と旧generationの保持先。旧treeは追加ファイルを含めて保持し、選択・有効化は変更しない。 |
| `polaris decision plan-download <profile> [--repair]` | 固定配布元からの取得・導入計画を確認する。 | 取得URLと転送許可先、size/SHA、用途、license/NOTICE、導入先、取得容量と必要ディスク空き。通信・copyなし。固定download情報がない場合は失敗。 |
| `polaris decision download <profile> [--repair] [--yes]` | 今回の計画を確認後、artifactを取得・検証して導入する。 | 計画JSON、`y/n`、導入または取消/失敗の通知。HTTPSの固定配布先だけに接続し、一時保存した両ファイルのsize/SHAを照合する。既存generationがあれば`--repair`なしでは拒否し、修復時は旧treeを保持する。選択・有効化・推論なし。 |
| `polaris common-tests <plan.json\|->` | JSONで指定したブラックボックス試験をsandbox内で実行する。 | JSONの`result: passed / failed / unavailable`、試験対象、各case、fallbackの要否等。指定プログラムの実行と一時領域の使用を伴う。 |
| `polaris completion <shell>` | シェル補完scriptを生成する。 | script本文をstdoutへ出す。対応はbash/zsh/fish/elvish/powershell。導入は別操作。 |
| `polaris --help`、`polaris help [command]` | CLIの構文を調べる。 | コマンド・引数・説明。`polaris mcp configure --help`のように絞れる。 |
| `polaris --version`、`polaris -V` | バージョンを見る。 | `polaris 1.0.0`。TUIに`/version`はない。 |

一般的な成功は終了コード0、操作失敗は1、構文誤りは2。`sandbox`は子の終了コードを返す。`doctor`の終了コード0は、モデル等への実接続成功を示さない。

判断器の管理コマンドは2026-10-21の実装を反映している。現在の受入済みprofileは0件のため、Clefの取得・導入はまだ利用できない。`download`の確認はその場で作る計画に対して行い、`plan-download`の表示を後から承認する方式ではない。必要ディスク空きは取得用と導入用の一時領域・予備を含む。取得・検証・導入全体の期限は2時間で、期限後は取消を伝えて処理の終了を待つ。終了状態を確定できない場合は`decision verify`で再確認する。通常起動や初回判断で自動取得は行わない。

### よく使う起動オプション

| オプション | 役割 |
| --- | --- |
| `--model <ID>` | モデルを指定。省略時は`POLARIS_MODEL`、未設定なら`gpt-6-astra`。 |
| `--account <登録ID>` | 送信に使う登録アカウントを指定。 |
| `--effort low\|medium\|high\|xhigh\|max` | rootの推論強度を固定。省略時、TUIは自動、非対話実行はxhigh。`ultra`は受け付けない。 |
| `--max-turns <n>` | 1runのturn上限。既定20。 |
| `--sandbox read-only\|workspace-write\|full-access` | 実行隔離を指定。既定workspace-write。 |
| `--approval never\|on-request\|always` | 承認方針を指定。既定on-request。 |
| `--audit <path>` | 監査ログの保存先。 |
| `--remember` | 圧縮前履歴を保存し、project別の検索索引を作る。 |
| `--tool-memory off\|history\|retrieval` | 大きなツール結果の保存・参照方式。既定off。 |
| `--tool-memory-embedding-url <URL>`、`--tool-memory-embedding-model <ID>` | ツール記憶の埋め込み接続先とモデル。必ず組で指定。 |
| `--compact-at <n>` | 自動圧縮を始める推定token数。指定時は1000以上。 |
| `--history-mode legacy\|strict10\|structured10` | 会話履歴の方式。strict10には固定したローカル埋め込み設定が必要。 |
| `--resume <ID>`、`--fork <ID>` | 保存会話を再開、または別UUIDへ分岐する。同時指定不可。 |
| `--workflow-mode manual\|adaptive\|off` | workflowの実行方式を指定。 |
| `--phase <段階>` | general/brainstorm/specify/implement/review/verify/deliverを指定。resume/forkとの同時指定不可。 |
| `--mcp-external` | MCPの実行・登録を許す起動時の上限。操作ごとの確認は別途行う。 |
| `--mcp-prompt <SERVER/PROMPT>`、`--mcp-prompt-arg KEY=VALUE` | 初回要求の前に使うMCP promptを1件選ぶ。引数は複数指定可能。 |
| `--web-search disabled\|cached\|live` | 現状はdisabledのみ利用可能。cached/liveは開始前に拒否。 |

トップレベルに`--provider`引数はなく、providerは環境変数等で設定する。`POLARIS_PROVIDER=codex`はChatGPT認証、`openai`はAPIキーを使う。`polaris status`・`polaris session`は存在せず、TUI内で`/status`・`/session`を使う。

### 認証・アカウント

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `polaris login` | ブラウザを使い、単一のChatGPT認証を保存する。 | `Logged in: <保存path>`。失敗時`Can't log in: <理由>`。 |
| `polaris logout` | 単一認証の保存資格情報を解除する。 | `Logged out: <path>`、または`Not logged in`。Codexの保存領域は変更しない。 |
| `polaris account list` | 複数アカウントの登録状態を表示する。 | `<登録ID>  利用可  <表示名>  利用枠: 未取得`等。未登録なら単一認証を使用中という案内。 |
| `polaris account login [--label <名前>]` | ブラウザ認証を行い、アカウントを新規登録する。 | 認可の案内→`登録しました: <登録ID>`。対話端末が必要。 |
| `polaris account reauth <登録ID>` | 登録済みアカウントを再認証する。 | `再認証しました: <登録ID>`。 |
| `polaris account logout <登録ID>` | 指定登録を解除する。実行中runは次の送信時に止まる。 | `解除しました: <登録ID>`。 |
| `polaris account label <登録ID> <表示名>` | 表示名を保存する。 | `表示名を設定しました: <登録ID>`。 |

利用枠の観測は別processから引き継がれず、一覧の「未取得」は残量0を意味しない。保護された保存領域を開けない場合は、`アカウントを開けません: <理由>`等を表示する。

### MCPの管理

`--project <dir>`で対象projectを指定できる。省略時は現在のディレクトリ。登録はアカウント共通で、有効・無効はproject単位。`list`も保存領域を準備する経路を通るため、ファイルを一切作らないとは限らない。

| コマンド | 役割 | 主な出力・副作用 |
| --- | --- | --- |
| `polaris mcp list` | 登録・有効状態・既知catalog・秘密の参照名を見る。 | 登録の版、`<server-ID> 有効/無効 ツール<n>件/カタログ未確認 <接続先>`、認証情報の参照名。秘密値は表示しない。 |
| `polaris mcp enable <server>`、`disable <server>` | 現projectの利用設定を変更する。 | `<server> をこのプロジェクトで有効/無効にしました。次の実行から反映します。`。 |
| `polaris mcp secret-set <参照名>` | stdinパイプの1行を秘密値として保存する。 | `<参照名> を保存しました。次の実行から使えます。`。端末への直接入力は拒否する。値をコマンド引数へ書かない。 |
| `polaris mcp secret-delete <参照名>` | 保存した秘密値を削除する。 | `<参照名> を削除しました。`。参照中などは拒否される。 |
| `polaris mcp check <server>` | サーバーへ接続し、ツールcatalogの取得と停止を確認する。 | `<server>: ツール<n>件`。通信、stdioサーバーの起動、保存認証の使用、索引更新を伴う。モデルは使わない。 |
| `polaris mcp setup [--yes]` | Codex/Claude設定から検出した候補を確認し、登録する。 | 候補cardと確認→`<名前>: 登録しました。次の実行から使えます。`、または非対応・未登録の理由。 |
| `polaris mcp materialize <名前> [--runtime <path>] [--yes]` | launcher形式のpackageを取得して固定する。 | 取得元・配置先・通信の確認→`<package> <version> を固定しました`と実行方法。downloadと保存を行う。登録は別途`setup`。 |
| `polaris mcp configure <server> --preset unity-local-v2 --source <絶対dir> [--yes]` | 既存Unity登録1件のruntimeを専用領域へコピーし、登録を更新する。 | コピー元・hash・除外・配置先・設定差分の確認→`<server> を構成しました（登録の版 <n>）。次の実行から反映します。`。起動・download・接続確認は行わない。 |

`configure`の詳細は[利用ガイド](usage.md#登録済みunity-mcpの固定配置)を参照。`check`の成功はcatalog取得の確認であり、Unity Editorなどの操作成功を示さない。

### 会話記憶の管理

`--project <dir>`対応。埋め込みURLとモデルは組で指定し、接続先はloopbackに限定する。検索・参照結果は過去の記録として扱う。

| コマンド | 役割 | 主な出力 |
| --- | --- | --- |
| `polaris memory import <file.jsonl>` | 現projectに一致するJSONLを取り込む。 | `<n>件を取り込みました`。 |
| `polaris memory search <query> [--session <ID>] [--limit <1..20>]` | 保存した会話を検索する。埋め込み設定時はqueryをローカルserverへ送りhybrid検索。 | JSONの`kind: historical_evidence`、過去記録というnotice、`results`。hitはsession/id/role/source/excerpt/score等。 |
| `polaris memory get <session> <id> [--start <byte>] [--max-bytes <byte>]` | 保存本文を取得する。 | JSONの`record`。範囲指定時は`range.start/end/total_bytes`。範囲取得は既定512、上限4096byte、UTF-8境界を検証する。 |
| `polaris memory source --session <UUID> <source-ID> [--start <byte> --end <byte>]` | v2要約の出典IDを照合し、原文を取得する。 | 検証済みの原文テキスト。start/endは組で指定する。 |
| `polaris memory embed <session> <id> --embedding-url <URL> --embedding-model <ID>` | 指定本文をローカルserverで埋め込み、vectorを保存する。 | `Embedding saved`。 |
| `polaris memory index <session> [--batch <1..100>] [--after <ID>] --embedding-url <URL> --embedding-model <ID>` | 会話をまとめて埋め込む。既定100件。 | `<n>件の埋め込みを保存しました。続きの開始ID: <ID>`。 |
| `polaris memory forget <session>` | 本文・vector・保管原文を削除し、その会話の再取り込みを禁止する。 | 削除件数、v2索引件数、再取り込みを禁止する旨。巻き戻しでは取り消せない。 |

`search`には任意で`--embedding-url <URL> --embedding-model <ID>`を追加できる。操作失敗時は`記憶操作に失敗しました: <理由>`をstderrへ出す。

### 複数端末の同期

同期は、remoteとの受信・送信、同期用領域の状態、projectへの反映を別々に扱う。C08は共有資格情報の束縛、C10は共有内容の送信を記録する操作IDである。CLIのjoin/C08とpush/C10は現状macOS Data Protection Keychainが前提。

| コマンド | 役割 | 主な出力・確認 |
| --- | --- | --- |
| `polaris sync status` | 参加と同期の状態を見る。 | `共有:`、`最後に合意した内容:`、`手元の記録:`、`解消待ち:`。保存領域を準備する場合がある。 |
| `polaris sync join --remote <HTTPS> --share-id <ID> --invitation-id <ID> --expires <UNIX秒> --account <Keychain名> [--exclude <名前>...] [--yes]` | 招待された共有に参加し、資格情報を束縛する。 | C08操作IDと確認card→`Credentials bound and share joined.`。 |
| `polaris sync join --rebind-credential --account <Keychain名> [--credential-choice <識別prefix>] [--yes]` | 共有資格情報を再束縛する。 | C08確認→`Credential rebinding confirmed.`。 |
| `polaris sync join --recover <操作ID> [--yes]` | C08の未完了操作を照合する。 | `Credential rebinding reconciled and completed.`等。 |
| `polaris sync join --abandon <操作ID> [再照合用引数] [--yes]` | C08操作を明示的に中止する。 | `Credential rebinding abandoned.`。必要な引数は`--help`で確認する。 |
| `polaris sync push [--yes]` | 対象を記録し、remoteへ送信する。 | C10確認card→`C10送信完了: operation=<ID> commit=<hash>`、または共有内容が一致している旨。 |
| `polaris sync recover <操作ID>` | C10の結果を、再送せず照合する。 | 完了・結果未確定などのC10結果。 |
| `polaris sync abandon <操作ID> [--yes]` | C10を再確認して中止する。 | `C10操作を照合して中止しました: ...（未送信の証明ではありません）`。 |
| `polaris sync push --recover <ID>`、`--abandon <ID> [--yes]` | 上記recover/abandonの別入口。 | 上と同じ。 |
| `polaris sync pull` | remoteから受信し、機械的に解ける変更を同期用領域で統合する。 | `受け取りました: <n>件の変更。projectへの反映は別の操作で行います。`。衝突時は双方を保持しpathと理由を表示。 |
| `polaris sync apply` | 合意済み内容をprojectへ書き込む。 | `<n>件を反映しました。復旧用の控えは<path>にあります。`、または反映済みの通知。未解消の衝突は拒否。 |
| `polaris sync leave` | この端末の参加を終了する。 | 参加終了、または未参加の通知。共有履歴と相手側は残る。 |

C08/C10の確定は端末での確認、または`--yes`を必要とする。結果未確定は未送信を意味せず、同じ送信を繰り返す根拠にはならない。

### 定型作業・比較・確認文・決定事項

| コマンド | 役割 | 主な出力・現在の制限 |
| --- | --- | --- |
| `polaris setting git [--project <dir>]` | Git配置を検出し、不足する`.gitignore`規則を追加する。 | Gitの検出結果と、追加した規則または追加不要の通知。対象は`.next/`・`node_modules/`・`coverage/`。Git init/commitは行わない。 |
| `polaris compare <folderA> <folderB> [folder...]` | 名前とサイズからフォルダの類似性を調べる。 | フォルダの組、類似度、共通件数、提案と理由。本文の読取、移動、削除は行わない。 |
| `polaris handoff --summary "<概要>" --check "<確認>"... [--project <dir>] [--run <世代>] [--attempt <ID>]` | 変更ファイルを基に、実機確認を頼む文面を生成する。 | 対象、変更一覧、概要、確認手順、結果の依頼。変更がなければ生成しない。人へは送信しない。 |
| `polaris spare [--allow] [--reserve <0..100>] [--candidates <n>]` | 余剰枠を使った依頼外の整理を開始できるか判定する。 | 未許可、または利用枠の観測がないため停止する通知。**現状のCLIでは整理を開始しない。** |
| `polaris decided "<1文>"`、`polaris decided add "<1文>"` | projectの決定事項を追記する。 | 時刻と文章、`<project>/.polaris/decisions.csv`のpath。 |
| `polaris decided`、`polaris decided list` | 決定事項を古い順で見る。 | 時刻・文章の一覧、件数、CSV path。0件なら追記の案内。 |

## モデルが呼ぶツールと画面の読み方

`⏺ read(...)`などは呼び出したツール、`→ ...`は結果の要約、`done`/`failed`はそのツール処理の結果である。表示が省略される場合もある。`done`だけでは、利用者の依頼全体が完成したとは判定できない。

| 基本ツール | 役割 | 返す結果の形 |
| --- | --- | --- |
| `read` | ファイルの指定行、またはフォルダ直下の項目を読む。 | 行番号付き本文、または`file`/`dir`等と名前の一覧。存在しないpathや拒否対象は理由を返す。保存結果・会話原文URIの参照にも使う。 |
| `write` | ファイル全体を書き込む。既存は上書きし、必要な親フォルダを作る。 | `wrote <n> bytes to <path>`、または失敗理由。 |
| `edit` | 指定した旧文字列が1か所だけ一致するとき置換する。 | `replaced 1 occurrence in <path>`。一致0件・複数件は失敗。 |
| `bash` | sandbox内でshell commandを実行する。 | stdout/stderr、または終了コード・policy・出力を含むエラー。通常32KiBで省略。 |
| `skill` | skillを検索し、名前が完全一致した場合は本文を読む。 | 検索候補の名前・説明、またはskill本文。`q=""`は一覧。 |
| `spawn` | 独立した作業を子agentへ委譲する。 | `type`・`ok`・`result`または`error`を持つJSON配列。 |

`find`・`grep`・`ls`は独立したツール名ではなく、必要に応じて`bash`から使うshell commandである。`read`も現在はフォルダ一覧に対応している。

| 条件付きツール | 役割 | 返す結果・現在の制限 |
| --- | --- | --- |
| `mcp_catalog` | MCP候補の検索・選択、resourceや保存chunkの読取。 | 候補、部分取得の有無、選択状態、出典付き本文、次offset等のJSON。 |
| `mcp_<hash>` | 選択済みの外部MCPツールを呼ぶ。 | 出典付きのtext/structured/resource等。大きい結果はhandleと抜粋。名前はcatalog identityから生成する。 |
| `mcp_setup` | MCP候補の検出と、承認した候補の登録。 | 候補JSONまたは登録結果。外部接続上限と承認に従う。root専用。 |
| `setting` | 内部の環境構築計画と、許可されたGit除外規則の整理。 | 計画やwritten/reason等のJSON。package導入は行わない。 |
| `computer` | macOS appのaccessibility候補を読み、操作可否を示す。 | app名・候補件数・目標・候補text。**現状は操作selector未接続で、実操作は行わない。** |
| `state` | workflow状態・差分を読み、revision付きで遷移する。 | phase/revision/承認・検証状態等のJSON。 |
| `workflow_artifact` | routine変更契約の提案や、不変artifactの読取。 | artifact参照や本文、workflow revision等のJSON。採否・ID・hash・公開はhostが管理する。 |
| `verification` | hostが固定した検証checkpointを準備・実行する。 | checkpoint IDとprepared/recorded/rejected等。モデルにはprepare/runのみを公開し、publishはhost専用。 |
| `clarify` | 利用者への質問を記録し、そのturnを終える。 | 最大3問と、質問だけを返すよう促す結果。回答は次の通常入力として受ける。 |

使えるツールはworkflowの段階、権限、hostの接続状態によって変わる。すべてが常時使えるわけではない。内部helper用の`--confined-*`や、host発行のauthority引数は通常の利用者操作には含めない。

## 現在の提供状況で注意する点

- バージョンはシェルで`polaris --version`。TUIの`/version`・`/usage`・`/effort`は存在しない。
- `/web-search`・`/models`・`/browse`・`/attachment`・`/recipe`・`/handoff`・`/remember`・`/account`は削除した。モデル選択は`/model`、macOSでのファイル所在の表示は会話中のファイル名へのCtrl-click（端末対応時はCmd-click）を使う。エージェント内部のWeb検索経路と提供条件は維持している。
- `polaris setting nextjs`と`/setting nextjs`は廃止した。環境構築は`polaris env set <profile>`／`/env set <profile>`へ移した。旧コマンドの別名ではなく、固定計画の確認と専用runtimeによる導入を行う。
- `read`や`skill`はツール名で、`/read`・`/skill`というコマンドではない。
- `/model`で変更するのはモデル。推論強度の固定指定は起動時の`--effort`で行う。
- `/subagents cancel`は停止要求の受付。終了状態は同じchild/attemptに対する`wait`で確認する。`outcome_unknown`は結果未確定を表す。
- 巻き戻しは空の入力欄で待機中にEscを2回押す。正常完了して保存された対象を選び、変更予定を確認し、`r`で確定する。中断したturnは候補に含まれない。
- `--web-search cached/live`は現在利用できない。`spare`も、名前から想定される一連の作業はまだ実行しない。
- この一覧の確認では、認証・MCP・モデルへの実接続や、状態を変更するコマンドの再実行は行っていない。

## 実装の参照先

- TUIの名前・alias・構文：[slash.rs](../crates/polaris-tui/src/slash.rs)。画面とdispatch：[lib.rs](../crates/polaris-tui/src/lib.rs)、[render.rs](../crates/polaris-tui/src/render.rs)。
- 会話設定・goal・原文履歴：[session_commands.rs](../crates/polaris-tui/src/session_commands.rs)。実行・subagents：[runtime_commands.rs](../crates/polaris-tui/src/runtime_commands.rs)。
- TUIのローカル変更：[local_actions.rs](../crates/polaris-tui/src/local_actions.rs)、管理操作：[workspace_commands.rs](../crates/polaris-tui/src/workspace_commands.rs)。
- TUIの認証・MCP・記憶：[account.rs](../crates/polaris-tui/src/account.rs)、[mcp_commands.rs](../crates/polaris-tui/src/mcp_commands.rs)、[memory_commands.rs](../crates/polaris-tui/src/memory_commands.rs)。
- CLIの引数・入口：[main.rs](../crates/polaris-cli/src/main.rs)。管理処理：[mcp.rs](../crates/polaris-cli/src/mcp.rs)、[memory.rs](../crates/polaris-cli/src/memory.rs)、[sync.rs](../crates/polaris-cli/src/sync.rs)、[account.rs](../crates/polaris-cli/src/account.rs)。
- ツールの定義：[polaris-tools/lib.rs](../crates/polaris-tools/src/lib.rs)。公開条件と実行：[agent.rs](../crates/polaris-core/src/agent.rs)。
- 構成・保存先・導入方法：[利用ガイド](usage.md)。
