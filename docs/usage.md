# 利用ガイド

各コマンドの役割、表示内容、現在の対応範囲は[コマンドの役割と出力](command-reference.md)を参照。

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

one-shot実行の推論強度は、省略時に`xhigh`を使う。対話TUIでは省略時に自動判定し、`--effort`で明示した値が優先される。APIにはモデル名と推論強度を別々に渡す。CLIが受け付ける値は`low`・`medium`・`high`・`xhigh`・`max`の5つである。`max`はResponses API限定だが、主経路が`/responses`なので使える。この設定はroot実行自身の強度であり、委譲先の強度は`[tier.*]`が決める。

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

## 型付き判断器の設定

`[decision]`は通常の会話モデルとは別の判断器を選ぶ設定で、既定は無効である。既存のJevとSemIfは引き続き選択できる。モデルやhelperが見つかっただけでは有効にならない。

| `backend` | 指定する資源 | 現在の扱い |
| --- | --- | --- |
| `jev` | 任意の`model`・`base_url`と、環境変数`POLARIS_JEV_API_KEY` | 既存のHTTP経路。明示的に有効にした場合だけ使用する。 |
| `semif` | `helper`への絶対path | 既存のローカルhelper経路。Clefの導入状態にかかわらず維持する。 |
| `clef` | `model`にpackage登録済みの固定profile ID | macOSの管理対象generationを使う。`helper`・`base_url`や任意の実行ファイルpathは指定できない。 |

Clef/Clef-flashはモデル、量子化、runtime、device、割当contextごとに受入記録を持つ。**現在の本番lockには受入済みprofileがないため、Clefはまだ通常実行に使えない。** IDを設定するだけで受入済みにはならず、起動時にweightsを自動取得することもない。未導入・未検証のClefを選んだ場合は理由を表示し、Jevへ勝手に変更しない。Clefのanswer finalizerも未対応である。

`polaris doctor`の容量帯と候補順は推薦の目安で、実行可能という判定ではない。管理対象CPU workerの起動直前には、空いている物理メモリから保守的な予算を求め、profileのpeakと予備量を確認する。CPUの使用量をGPUへ換算せず、Metalのdevice別容量観測が未対応の間は、そのprofileを実行しない。

8GB程度・16GB程度・24GB以上という容量帯で、利用できるbackendを固定しない。Clef/Clef-flash、SemIf、Jevは、利用者の選択と各backendの実行条件に従って使用する。

判断と補助モデルによる候補選択を端末内に限定したい場合は、`[decision]`に`local_only = true`を指定する。既定は`false`である。`true`の場合は通信制限を確認できる判断器と補助モデルだけを許可し、判断失敗後も外部の補助モデルへ送信しない。loopbackの接続先やローカルhelperというだけでは、外部通信しない保証にはならない。現在のJev HTTP経路と、通信を制限していないSemIf helperは、この指定時には使用できない。管理対象Clefも、受入・導入・容量の条件を別途満たす必要がある。

利用者設定の`local_only = true`は、project設定の`false`では解除できない。この設定が制限するのは判断と補助的な候補選択であり、通常の会話モデルやMCPツール自体の通信許可は別に設定する。

### 型付きbackendの切替

型付きbackend間の切替は、利用者の`~/.polaris/config.toml`に明示した場合だけ行う。project設定から切替先を追加することはできない。次は登録済みClef profileを主に使い、未導入・利用不能などの場合にJevへ切り替える設定例である。`REGISTERED_PROFILE_ID`は実際の登録IDへ置き換える。本番の受入済みClef profileが0件である点は、この設定でも変わらない。

```toml
[decision]
enabled = true
backend = "clef"
model = "REGISTERED_PROFILE_ID"

[decision.fallback]
purposes = ["routing"]
max_attempts = 2
attempt_timeout_ms = 60000
total_timeout_ms = 120000

[[decision.fallback.targets]]
backend = "jev"
model = "jev-latest"
base_url = "https://api.typesafe.ai/v1"
```

`targets`は1〜3件を順番に指定する。`max_attempts`は主backendを含めた試行数で、1から候補総数まで。各試行と全体の時間上限を必須とし、合計は最大300000msである。切替設定時のJevは、backend内で追加の再試行を行わない。起動時に候補の接続先・モデル・用途・上限を表示する。各接続先にはstateと質問を送り、依頼・会話やファイル由来の内容・候補の説明等が含まれる場合がある。

用途は`routing`（候補選択・委任先・継続判定等の通常の型付き判断）だけに対応し、単発のanswer finalizerには適用しない。利用不能・入力超過・時間切れは次候補へ進めるが、認証失敗・設定不正・不正回答・取消では進めない。`local_only = true`と併用する場合は、全候補で通信制限を確認できる必要があり、現在のJev/SemIfを含む切替設定では型付き判断器を有効にしない。fallback設定がなければ、他の型付きbackendへ自動で切り替えない。

切替先へ主backendの校正を流用しない。未校正モデルによる候補絞込みは候補を維持し、判断の呼出自体を省く場合がある。既存の生成モデルによる候補選択は、型付きbackend間の切替とは別の段階である。

`polaris decision list`は登録済みprofile IDと容量の推薦をJSONで表示する。現在の登録は0件である。登録は導入や実行可否の確認ではなく、この操作でJev/SemIfの設定は変わらない。

受入済みprofileと配布情報が登録された後は、手元の固定artifactを次の管理コマンドで導入できる。`PROFILE_ID`と2つの絶対pathを対象に置き換える。

```sh
polaris decision plan PROFILE_ID --worker /absolute/path/worker --model /absolute/path/model.gguf
polaris decision import PROFILE_ID --worker /absolute/path/worker --model /absolute/path/model.gguf
```

`plan`は取得元、license/NOTICE、component一覧、size/SHA、導入先、必要なcopy領域を表示するだけで、ファイルを取得・配置しない。`import`は新しい計画を表示して`y/n`を求める。非対話実行では、その選択を明示承認する`--yes`が必要である。以前の計画や確認記録を再利用せず、導入直前にも固定lockと導入先を照合する。配布情報が不足するprofileは導入できない。

導入はnetworkを使わず、指定ファイルの全size/SHAを照合して新しいgenerationへcopyする。取消・破損・競合時は既存generationを残す。成功してもbackendの選択・有効化や推論は開始しない。現在の空lockで`plan`・`import`を実行しても、受入未完了の案内で終了する。

macOSインストーラの「モデル」ページにも「Clef判断器の導入（任意）」がある。「登録profileを確認」で同梱CLIの登録と容量推薦を読み、登録がある場合だけprofileと手元のworker/modelファイルを選べる。表示された取得元、license/NOTICE、component、size/SHA、導入先を確認して「この計画で導入」を押す。GUIが起動した同じCLIプロセスへ今回の計画hashを返すため、別の計画への確認流用は行わない。確認待ちは120秒で、対象を変えた場合や期限切れ時は計画を作り直す。現在の登録は0件であり、導入ボタンは提供されない。

## ChatGPT認証

```sh
polaris login
POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"
```

`polaris login`はブラウザを開き、`http://localhost:1455/auth/callback`で認可を受け取る。資格情報は`~/.polaris/auth.json`に0600で保管し、`~/.codex/`には読み書きしない。`polaris logout`はPolarisが保存した資格情報を削除する。`ls -l ~/.polaris/auth.json`で`-rw-------`であることを確認できる。

期限切れの更新経路を手で確認する場合は、テスト用の資格情報に限り`expires_at`を過去のUnix秒または`null`へ変更し、再実行後に`expires_at`、`account_id`、0600が維持されることを確認する。JSONを壊さないこと。

## macOSデスクトップ

SwiftUIの作業画面は、選択したプロジェクトと会話を専用の保存先へ結び付ける。設定ファイルはプロジェクトの外に置き、登録時のディレクトリidentityと再接続時のidentityが異なる場合は、保存済みの会話を維持したまま接続を拒否する。同じ設定を別のアプリから開くことも拒否する。終了時は下書きと復旧状態の保存を完了してから終了する。

会話、task、child、ファイル、Gitの各欄は保存済みの状態と保護された読取結果から表示する。ファイル本文と添付は、明示的に選択したUTF-8テキストだけを取り込む。添付は内容を確認してから保存または送信でき、4件、各64 KiB、合計128 KiBまでに制限する。画像とバイナリは扱わない。音声入力はmacOSの許可を得た場合に下書きへ追加するが、録音の停止だけで送信はしない。

導入済みのOllamaまたはLM Studioは、アプリから明示的に更新したときに一覧へ反映する。到達できないendpointは未導入とは判定しない。主会話と役割別のローカルモデルは保存後に接続を確認してから実行へ使う。ツール必須の役割にはツール対応を確認できないモデルを割り当てず、未設定の役割をクラウドモデルへ自動的に切り替えない。役割保存の結果が不明な場合は同じ保存を再送せず、保存済み状態を照合する。

この画面は未署名のローカル確認用であり、配布版ではない。組立てと起動方法は[macOSデスクトップ](../apps/macos/README.md)を参照。

モデルの編集は隔離した作業領域に保存する。元のプロジェクトへの反映は、完了後に「保留中の変更」を全件確認して許可する。実行の完了と反映の成功は別に表示し、期限切れや外部編集との競合では元ファイルを上書きしない。操作の詳細は[元ファイルへの変更反映](../apps/macos/README.md#元ファイルへの変更反映)を参照。

## TUIと会話

`--prompt`を省略すると対話TUIを起動する。資格情報がなく、`POLARIS_PROVIDER`が未設定または`openai`なら、ChatGPTサインインまたはOpenAI APIキー入力を選ぶ画面を出す。入力したAPIキーは`~/.polaris/api_key.json`へ0600で保存する。`codex`を明示した場合とone-shot実行では、この選択画面は出ない。

通常のlegacy起動は空の会話から始まり、送信済み会話を`~/.polaris/sessions/<会話id>.jsonl`と`<会話id>.meta.json`へ保存する。workflowまたはstrict10を有効にした会話は`~/.polaris/sessions-v2/<UUID>/`を使う。v2はrawイベント、世代付きsnapshot、状態markerを保存するため、ツール呼び出しと中間結果も回復対象になる。起動だけで終了したlegacyの空会話は保存しない。待機中は`Ctrl-C`を2回押して終了する。

保存済みv2会話では、待機中に入力欄を空にして`Esc`を2回押すと巻き戻し候補を選べる。対象を選んで変更予定のファイルを確認し、`r`で確定すると、そのユーザー入力の直前の作業ファイルと会話文脈を新しい会話に復元する。元の会話は残るが、作業ファイルと一致しない会話への送信は拒否される。対象外の変更、外部編集との衝突、保存が不完全な境界は選べない。捕捉が2秒の上限を超えたターンも巻き戻し対象外となる。

ヘッダーにはモデル、プロバイダ、作業ディレクトリ、累積使用量を表示する。ツール呼び出しは`⚙ ツール名(引数)`、結果は`→ 要約`として表示し、長い表示だけを約200文字で打ち切る。Markdownの太字、インラインコード、コードブロックを整形する。実行中は経過時間と中断キーを表示し、`esc`は待機を中断するが、すでに起動したOSサブプロセスを強制終了しない。

失敗ターンの使用量は累計表示へ反映されない。プロバイダがusageを返さないときは`0`と表示する。履歴ペインはスクロールバックを持たず、最新メッセージだけを表示する。

`/model`はホスト型モデルと`Local model`を選ぶピッカーを表示する。`Local model`を選ぶと、接続済みのOllamaとLM Studioからモデルを取得し、次の一覧で選択できる。ツール対応を確認できたモデルだけを採用し、次のターンから反映する。推論強度はこのピッカーでは選ばない。`/permissions`では起動時の承認境界を超える選択を拒否し、固定のsandboxと再起動用の引数を表示する。`/skills`は閲覧専用である。

`/resume`の一覧は、現在の作業ディレクトリを最初に置き、ほかのディレクトリは最後の会話日時の新しい順に並べる。各行には相対時刻、メッセージ数、最初のユーザーメッセージの60文字プレビューを表示する。30日を超えた会話は絶対日時で表示する。legacyでは壊れた行より前までを読むが、v2の破損・世代不一致は原文を切り捨てず拒否する。

### スラッシュコマンド

`/`で候補を絞り込み、`↑`/`↓`とEnterで実行する。状態照会や設定変更はローカルで処理し、`/compact`と`/review`はモデルを使う。Linux限定の`/apply run`もモデル実行を伴う。全コマンドの構文・出力は[コマンド一覧](command-reference.md)にまとめる。

| コマンド | 内容 |
| --- | --- |
| `/help`、`/status`、`/pwd` | ヘルプ、現在の状態、作業ディレクトリを表示する。 |
| `/compact` | 古い履歴を要約へ置換する。閾値超過時は自動実行もする。 |
| `/phase` | workflowが有効なら現在の段階を表示し、`/phase implement`のように次ターンから使う段階を保存する。 |
| `/skills` | 検出したskillを一覧表示する。 |
| `/new`、`/resume`、`/fork`、`/clear` | 会話を新規作成、再開、複製、現在epochを消去する。`/clear`でも過去の証拠は保持する。 |
| `/init` | カレントディレクトリに`AGENTS.md`の雛形を作成する。既存時は何もしない。 |
| `/model`、`/permissions` | モデル、起動時の境界内で承認ポリシーを選び直す。推論強度の固定は起動時の`--effort`で指定する。 |
| `/mcp` | 登録・外部接続上限と認証情報の参照名を表示する。`setup`で検出・登録、`check <server>`でカタログ取得、`enable <server>`・`disable <server>`でプロジェクトの有効化を変更する。 |
| `/mcp materialize <名前>` | 検出したパッケージを確認後に取得・固定する。登録は別途確認する。取得処理の終了までは待つ必要がある。 |
| `/mcp secret set`、`/mcp secret delete <参照名>` | 専用画面で秘密値を入力し、または保存済みの認証情報を確認後に削除する。`set`の引数へ値を書かない。値も文字数も表示せず、Esc・Ctrl-Cで入力を取り消せる。 |
| `/subagents list`、`/subagents wait <child-id> <attempt-id>`、`/subagents cancel <child-id> <attempt-id>` | 実行中の子を一覧からIDで選び、状態を取得するか、確認後に停止を要求する。停止要求の受付は終了証明ではなく、`outcome_unknown`は結果未確定を示す。 |
| `/memory import <JSONL>`、`/memory search <query>`、`/memory get <session> <id>`、`/memory source <session> <id>`、`/memory forget <session>` | 記録の取り込み、検索・参照、確認後の記憶削除を行う。取り込みと削除は巻き戻しでは取り消せない。 |
| `/diff`、`/review` | 変更点を表示し、またはレビュー指示を通常のモデルターンとして送る。 |
| `/export` | 会話をMarkdownへ書き出す。ファイル名省略時はタイムスタンプ付きの名前を使う。 |
| `/logout`、`/quit`、`/exit` | 保存済み認証を削除し、または終了する。 |

`/resume`は作業ディレクトリ別に会話をグループ化し、現在のディレクトリを先頭に表示する。選んだ会話への送信は既存ファイルへ追記する。

MCPの外部接続は起動時の`--mcp-external`が上限となる。カタログ取得・登録・パッケージ取得は、上限や承認設定が拒否した場合には実行しない。秘密値は通常入力・履歴・モデルへ送らず、専用入力画面でのみ扱う。

### 登録済みUnity MCPの固定配置

CLIの`mcp configure`は、有効な既存stdio登録1件を対象に、ローカルのUnity MCPを専用のversion directoryへコピーする。対応するpresetは`unity-local-v2`だけで、登録の唯一のentry引数が指定元の`build/index.js`と一致する必要がある。

```sh
polaris mcp configure <server-id> --preset unity-local-v2 --source /absolute/path/to/unity-mcp-ts
```

表示されたコピー元・配置先・ハッシュ・登録差分を確認して承認する。非対話実行では`--yes`で同じ内容を承認する。承認前に配置先や管理操作の記録は作らない。

コピー対象は`build/`、`node_modules/`、`package.json`と存在するlockfileである。`node_modules/.bin`と、依存package配下の固定リストにある同名directoryを除外し、実在する除外先を確認画面とmanifestへ列挙する。検証済みの特定のesbuild hardlink pairだけを独立した通常fileへ展開する。別の構成や検証できないfileは拒否するため、その場合は元の登録を確認する。

配置の完了後に登録を更新し、次の実行から新しいcwd・entry、`auto_2024_compat`、loopback限定の通信設定を使う。構成中にdownload、プログラムの起動、接続確認は行わない。旧runtimeと未参照のコピーは保持する。途中で結果不明となった操作は繰り返さず、管理操作の記録と保存済み状態を確認する。

構成後は`polaris mcp check <server-id>`でカタログを確認できる。カタログ取得の成功はUnity Editorへの実接続を示さない。`polaris mcp disable <server-id>`は現在のprojectでの利用を止める操作で、旧登録の復元ではない。

### AGENTS.mdの変更反映

macOSのCLIとTUIでは、グローバルの`~/.polaris/AGENTS.md`と起動時のプロジェクト直下の`AGENTS.md`を、親エージェントの各モデル要求の直前に確認する。変更はその要求から反映する。同じターンのツール実行後も確認するため、再起動や次のユーザー入力を待つ必要はない。定期ポーリングは行わず、送信済みの要求は変更しない。

取り込むのは従来どおり`Always on`節で、既存のトークン上限を維持する。本文が変わらない確認や対象節以外の編集によって、固定プロンプトに時刻・更新番号を追加することはない。子エージェントへの新規注入、別ディレクトリの探索、実行権限の変更は行わない。

読取は各ファイル256KiB以内。確認できた不存在は規則の削除として扱うが、読取不能、不正UTF-8、上限超過、検出した読取中変更、symlinkや保護対象ファイルへの差し替えでは要求を停止する。旧規則や空の規則へ黙って切り替えて続行しない。

### files.mdの自動更新

files-md-writer型を導入している場合、rootのツール変更後に対象ディレクトリの`files.md`を自動更新する。**既定は無効である。** 生成した`files.md`を要求へ載せる経路は無く、ディレクトリが増えるたびにsubagentを起動する分だけが残るため、2026-09-19に既定をfalseへ変えた。必要なプロジェクトでは、グローバルまたはプロジェクト設定で有効にする。

```toml
[files_md]
auto_regenerate = true
```

この設定はroot起点の自動更新だけを止める。`spawn`による明示的なsubagent実行は維持される。

コマンドの一覧は`/help`で確認できる。未対応の操作は明示的に拒否する。`/usage`は`/status`のトークン表示と重なるため設けない。

## subagent

`spawn`の型、設定、出力検証、深さの制限は[subagent](subagents.md)にある。

## 隔離実測のbroker連携

外側のnative sandboxからツールを実行する開発用連携では、launcherが`POLARIS_SANDBOX_BROKER`と`POLARIS_SANDBOX_BROKER_TOKEN`を組で設定する。未指定なら通常の実行経路を使う。接続先は同じlauncherが渡す`HOME`以下の`.local/share/codex-benchmarks/<32桁の小文字16進ID>/broker.sock`に限定し、非絶対パス・親ディレクトリ参照・リンク経由の接続を拒否する。任意の接続先rootを許可する設定はない。

`HOME`・socket・tokenは信頼するlauncherの設定として扱う。実際の接続先、ファイル範囲、ツールの実行権限は外側のsandboxとbrokerが独立して制限する。公開パッケージに共通実測ランナー自体は同梱しない。
