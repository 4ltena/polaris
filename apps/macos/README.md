# macOSデスクトップ

このディレクトリは、macOS 13以降で動作するSwiftUI版PolarisのSwift Packageである。Polaris本体と、初回設定・依存資源・本体配置を担当する独立インストーラを生成する。ローカル生成物は未署名で、署名・公証・公開は別工程である。

アプリは選択したプロジェクトの外側に会話と設定を保存する。登録済みフォルダのidentityが変わった場合、保存済み会話を残して再接続を拒否する。同じsettings pathを二つのアプリから同時に使うこともできない。終了は下書きと復旧結果の保存を確認してから完了する。

## 組立て

macOS 13以降、Swift 6、現在のcheckoutでビルドしたservice helperが必要である。モデル実行を確認する場合は、実行helperも明示する。いずれも絶対パスで渡す。

```sh
cargo build -p polaris-desktop-service --bin polaris-desktop-service
cargo build -p polaris-cli --bin polaris

python3 apps/macos/scripts/assemble-app.py \
  --service-helper "$PWD/target/debug/polaris-desktop-service" \
  --execution-helper "$PWD/target/debug/polaris" \
  --destination /absolute/path/PolarisDesktop.app

python3 apps/macos/scripts/assemble-installer.py \
  --service-helper "$PWD/target/debug/polaris-desktop-service" \
  --execution-helper "$PWD/target/debug/polaris" \
  --destination "/absolute/path/Polaris Installer.app"
```

`--execution-helper`を省略すると、実行helperを同梱しないアプリになる。Rustツールチェーン、llama.cpp、e5用の隔離Python実行環境、strict10用multilingual-e5-small、事前選定用multilingual-e5-baseはアプリへ同梱しない。独立インストーラが、選択したフォルダへの`Polaris.app`配置と、固定manifestから`~/.polaris`配下への資源取得を行う。隔離Pythonにはtorch、transformers、tokenizers、safetensorsと、アーキテクチャに合うNumPyを固定して導入する。検証済みの絶対パスは`~/.polaris/managed-resources.toml`へ保存し、利用者が`config.toml`で明示した値を優先する。システムのRust・Python・PATHは変更しない。組立てスクリプトは既存の出力先を上書きせず、起動や署名も行わない。

インストーラは6段階で、インストール先、GPT・Jev、Ollama・LM Studio・llama.cpp、任意のGGUF、作業フォルダ、権限の希望、テーマを保存する。既存の`Polaris.app`を置き換える場合は実行直前に確認し、一時退避から復旧できる形で更新する。Polaris本体に旧初回セットアップ画面はなく、完成済み設定がない場合はインストーラの実行を案内する。

同梱するスキルはGit管理された`skills/`、子エージェント定義は`agents/`から取得する。ビルド元の`.polaris/skills`やホームの個人設定はアプリへコピーしない。これらのローカル設定を作らずに、新しく取得したソースから組み立てられる。実行時のプロジェクト・ホームからのスキル探索は従来どおりである。

使い捨ての設定ファイルで起動するには、絶対パスを`--settings-path`へ渡す。

```sh
open -n /absolute/path/PolarisDesktop.app --args \
  --settings-path /absolute/path/review/settings.json
```

設定ファイルとそのworkspace保存先はプロジェクトの外に置く。既存の設定を確認したい場合も、同じsettings pathを同時に二つのアプリで開かない。

## 作業画面

インストーラでプロジェクト、希望する権限、クラウドまたはローカルの主モデルを選ぶ。Polaris本体は保存済み設定を読み、会話へ接続する。モデルまたは権限を変更すると、既存の所有者を終了して保存した設定へ接続し直す。

会話欄は保存済みの会話、task、childと実行状態を表示する。ファイルとGitの欄は、プロジェクト内の保護された読取結果だけを表示する。秘密名、登録済み秘密、link、生成物、バイナリ、範囲外のパスは読取対象にしない。Gitの状態や履歴を取得できないときは、利用不可として表示する。

添付は選択したUTF-8テキストだけを内容確認のために読み込む。内容を確認した添付だけが保存または送信でき、最大4件、各64 KiB、合計128 KiBまでである。画像とバイナリは対象外である。音声入力はmacOSの権限を必要とし、認識した本文は送信前に下書きへ追加する。

ローカルモデルの一覧はOllamaとLM Studioのendpointを明示更新して取得する。endpointに到達できないことは未導入の証拠ではない。役割別の割当は保存結果を照合してから反映し、ツール必須役割にツール対応を確認できないモデルは設定できない。未設定役割をクラウドへ自動で切り替えることはない。

### 会話の記憶

履歴設定で「従来の履歴」または`strict10`を選ぶ。旧設定は従来方式として読み、主モデルを変更しても選択した履歴方式を保持する。変更時は現在の実行所有者の終了を確認し、保存・bootstrapの照合後に接続し直す。

`strict10`は全原文をv3保存先に残し、モデルへの直接投入を直近10実ユーザーターンに限る。それ以前は要約とローカル埋め込みから検索する。原文の表示履歴は削らない。検索は同じプロジェクト・会話に限定し、主回答・要約・ローカル埋め込みの使用量を実行詳細へ別々に表示する。欠測は「未取得」とする。

主接続はCodexまたはOpenAIが必要で、同じ認証系の専用`gpt-6-astra/medium`で要約する。Polarisインストーラは、固定した隔離Python実行環境とmultilingual-e5-smallを取得し、strict10へ自動接続する。必要なら、OSユーザーの`~/.polaris/config.toml`にある`[embedding]`で自動設定を上書きできる。形式と取得上限は[コンテキスト効率化](../../docs/context-efficiency.md#strict10とsessions-v2)を参照。資源が不足または不一致なら理由を表示し、主要求を開始しない。ローカル主接続は従来方式で利用する。

実GUIから固定12入力を送り、11番目の原文参照と再起動時の履歴復元を確認した。12番目で出典情報込みの予算超過による検索不具合を検出し、上限を維持して修正した。修正後の自動回帰、アプリ接続、12ターンの復元は成功。追加実送信は省略したため、修正後の実モデル受入は未検証である。

### 元ファイルへの変更反映

モデルの編集は隔離した作業領域で行う。実行が完了しても、それだけでは元のプロジェクトを変更しない。「保留中の変更」で対象パスと変更前後のハッシュを全件確認してから、反映を許可する。これは本文の差分表示ではない。拒否した候補や期限切れの候補は反映しない。

許可の受付と反映結果の保存は別の段階である。「反映結果」の成否と書込み・削除・復旧の件数を確認する。元ファイルが候補作成後に外部編集されていた場合は競合として扱い、その内容を上書きしない。回答の成否が不明なときは許可を繰り返さず、保存済み状態を読み直す。結果が未記録の表示を反映成功とは判断しない。

## 検査

Swiftの自動試験は次で実行する。

```sh
swift test --package-path apps/macos
python3 apps/macos/scripts/test_assemble_app.py
python3 apps/macos/scripts/test_assemble_installer.py
```

これらはSwift側のロジックとアプリ組立ての自動試験であり、実画面の操作、実モデル応答、実録音、署名、配布形式を検証するものではない。
