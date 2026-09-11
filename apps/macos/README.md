# macOSデスクトップ

このディレクトリは、macOS 13以降を対象としたSwiftUI版PolarisのSwift Packageである。[v0.12.0 “Alrescha”](https://github.com/4ltena/polaris/releases/tag/v0.12.0)ではApple Silicon向けDMGを配布する。実モデル接続、ファイル・Git・作業表示、ローカルモデル、復旧、strict10の会話記憶を備える。

アプリは選択したプロジェクトの外側に会話と設定を保存する。登録済みフォルダのidentityが変わった場合、保存済み会話を残して再接続を拒否する。同じsettings pathを二つのアプリから同時に使うこともできない。終了は下書きと復旧結果の保存を確認してから完了する。

## 配布パッケージ

[polaris-0.12.0-arm64.dmg](https://github.com/4ltena/polaris/releases/download/v0.12.0/polaris-0.12.0-arm64.dmg)はApple Silicon、macOS 13.5以降向け。Intel Mac用・Universal版ではない。同梱Node.jsの最低OSに合わせ、ソース側のmacOS 13より配布版の要件を高くしている。

1. DMGを開き、上側の宇宙にあるPolarisを、下側の地球にあるApplicationsへドラッグする。
2. コピー終了後、Applications内のPolarisを開く。既存版が動作している場合は、通常終了してから置き換える。
3. 「ようこそ → GPT接続 → ローカルモデル → プロジェクトと権限 → テーマ → 確認して開始」の6ページを進める。接続設定は後回しにもできる。

Developer ID署名・公証はない。開発元を確認できない場合は[Appleの案内](https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unknown-developer-mh40616/mac)を参照。取得したDMGはReleaseの`polaris-0.12.0-SHA256SUMS.txt`と照合できる。

同梱するのはrelease構成のSwiftUIアプリ、service helper、実行helper、スキル2件、エージェント定義・Schema、Node.js 24.11.1と各ライセンスである。個人の認証・設定・会話やモデルキャッシュは含めない。npm、Python、Rust、Swiftなどの追加ビルド環境は同梱せず、ホストの環境を隔離実行へ自動流用しない。Git表示にはmacOSの`/usr/bin/git`を使うため、Apple Command Line Toolsが必要な環境もある。`strict10`は下記の固定埋め込み資源を別途設定する。

アプリはコピーした利用者の所有である必要がある。DMG上からの直接起動や、管理者がroot所有で配置したアプリは起動時の所有者検査を通らない場合がある。通常のFinderコピーを使い、システム用の`.pkg`としては配布しない。

## 組立て

macOS 13以降、Swift 6、現在のcheckoutでビルドしたservice helperが必要である。モデル実行を確認する場合は、実行helperと移設可能なtoolchain packageも明示する。いずれも絶対パスで渡す。

```sh
cargo build --locked -p polaris-desktop-service --bin polaris-desktop-service
cargo build --locked -p polaris-cli --bin polaris

python3 apps/macos/scripts/assemble-app.py \
  --service-helper "$PWD/target/debug/polaris-desktop-service" \
  --execution-helper "$PWD/target/debug/polaris" \
  --toolchain-package /absolute/path/to/toolchain \
  --destination /absolute/path/PolarisDesktop.app
```

`--execution-helper`と`--toolchain-package`を省略すると、実行helperを同梱しないアプリになる。組立てスクリプトは既存の出力先を上書きせず、起動や署名も行わない。

通常はdebug構成。最適化する場合はCargoに`--release`を付け、`target/release`のhelperを渡し、組立てにも`--configuration release`を指定する。

配布画面の背景とアイコンは、既存SVGから次で生成する。Finderでは640×520の背景に対し、PolarisとApplicationsを同じ横位置で上下に配置する。

```sh
swift apps/macos/scripts/render-installer.swift /absolute/path/artwork \
  "$PWD/apps/macos/Sources/PolarisDesktop/Resources/polaris-banner-white.svg"
iconutil --convert icns --output /absolute/path/artwork/Polaris.icns \
  /absolute/path/artwork/Polaris.iconset
sips --setProperty dpiWidth 144 --setProperty dpiHeight 144 \
  /absolute/path/artwork/background@2x.png
```

v0.12.0の配布版では`Contents/Resources/Polaris.icns`を`CFBundleIconFile`から参照し、`LSMinimumSystemVersion`を13.5とした。SwiftPMのリソースbundleは生成accessorに合わせて.app直下に置く。この配置はアプリ全体の署名に対応していない。署名・公証対応は別の変更として扱い、同梱helperの最終バイト列と`execution-helper.json`のSHA-256を常に一致させる。

DMG内の`Polaris.app/Contents/Resources/InstallerBackground.png`へ生成背景を置き、`Applications`を`/Applications`へのリンクにする。背景をアプリ内に格納することで、隠しファイル表示が有効でもインストール画面に背景用フォルダを出さない。Finderを閉じた状態で、`ds-store==1.3.3`と`mac-alias==2.2.3`を用意したビルド用Pythonから`write-dmg-layout.py /absolute/mounted/dmg`を実行すると、当該DMGの配置だけを保存する。利用者のFinder設定は変更しない。通常アンマウント後、`hdiutil convert`の`UDZO`形式で圧縮し、`hdiutil verify`と展開後のハッシュを確認する。

同梱するスキルはGit管理された`skills/`、子エージェント定義は`agents/`から取得する。ビルド元の`.polaris/skills`やホームの個人設定はアプリへコピーしない。これらのローカル設定を作らずに、新しく取得したソースから組み立てられる。実行時のプロジェクト・ホームからのスキル探索は従来どおりである。

使い捨ての設定ファイルで起動するには、絶対パスを`--settings-path`へ渡す。

```sh
open -n /absolute/path/PolarisDesktop.app --args \
  --settings-path /absolute/path/review/settings.json
```

設定ファイルとそのworkspace保存先はプロジェクトの外に置く。既存の設定を確認したい場合も、同じsettings pathを同時に二つのアプリで開かない。

## 作業画面

初回設定でプロジェクト、希望する権限、クラウドまたはローカルの主モデルを選ぶ。選択内容を保存してから「保存した設定を確認して接続」を実行すると、保存済みの会話へ接続する。モデルまたは権限を変更すると、既存の所有者を終了して保存した設定へ接続し直す。

会話欄は保存済みの会話、task、childと実行状態を表示する。ファイルとGitの欄は、プロジェクト内の保護された読取結果だけを表示する。秘密名、登録済み秘密、link、生成物、バイナリ、範囲外のパスは読取対象にしない。Gitの状態や履歴を取得できないときは、利用不可として表示する。

添付は選択したUTF-8テキストだけを内容確認のために読み込む。内容を確認した添付だけが保存または送信でき、最大4件、各64 KiB、合計128 KiBまでである。画像とバイナリは対象外である。音声入力はmacOSの権限を必要とし、認識した本文は送信前に下書きへ追加する。

ローカルモデルの一覧はOllamaとLM Studioのendpointを明示更新して取得する。endpointに到達できないことは未導入の証拠ではない。役割別の割当は保存結果を照合してから反映し、ツール必須役割にツール対応を確認できないモデルは設定できない。未設定役割をクラウドへ自動で切り替えることはない。

### 会話の記憶

履歴設定で「従来の履歴」または`strict10`を選ぶ。旧設定は従来方式として読み、主モデルを変更しても選択した履歴方式を保持する。変更時は現在の実行所有者の終了を確認し、保存・bootstrapの照合後に接続し直す。

`strict10`は全原文をv3保存先に残し、モデルへの直接投入を直近10実ユーザーターンに限る。それ以前は要約とローカル埋め込みから検索する。原文の表示履歴は削らない。検索は同じプロジェクト・会話に限定し、主回答・要約・ローカル埋め込みの使用量を実行詳細へ別々に表示する。欠測は「未取得」とする。

主接続はCodexまたはOpenAIが必要で、同じ認証系の専用`gpt-6-astra/medium`で要約する。OSユーザーの`~/.polaris/config.toml`に固定埋め込み資源を設定する。形式と取得上限は[コンテキスト効率化](../../docs/context-efficiency.md#strict10とsessions-v2)を参照。資源が不足・不一致なら理由を表示し、主要求を開始しない。モデルの自動取得は行わず、ローカル主接続は従来方式で利用する。

実GUIから固定12入力を送り、11番目の原文参照と再起動時の履歴復元を確認した。12番目で出典情報込みの予算超過による検索不具合を検出し、上限を維持して修正した。修正後の自動回帰、アプリ接続、12ターンの復元は成功。追加実送信は省略したため、修正後の実モデル受入は未検証である。

### 元ファイルへの変更反映

モデルの編集は隔離した作業領域で行う。実行が完了しても、それだけでは元のプロジェクトを変更しない。「保留中の変更」で対象パスと変更前後のハッシュを全件確認してから、反映を許可する。これは本文の差分表示ではない。拒否した候補や期限切れの候補は反映しない。

許可の受付と反映結果の保存は別の段階である。「反映結果」の成否と書込み・削除・復旧の件数を確認する。元ファイルが候補作成後に外部編集されていた場合は競合として扱い、その内容を上書きしない。回答の成否が不明なときは許可を繰り返さず、保存済み状態を読み直す。結果が未記録の表示を反映成功とは判断しない。

## 検査

v0.12.0ではSwift 236件と梱包試験6件が成功した。Git管理されたスキルとエージェント定義を使い、個人設定も既存のSwiftビルドキャッシュもない環境で組立てを確認している。strict10修正後の追加実モデル確認は省略した。

配布準備ではrelease構成の回帰を加えた梱包試験7件が成功した。最適化ビルド、4実行物のアーキテクチャとシステムライブラリ依存、helperの最終SHA、534ファイルのDMG内・移設先との一致、所有者・権限・link条件を確認した。圧縮DMGの`hdiutil verify`と、Finderでの背景・上下配置・ファイル名の可読性も確認済み。今回の配布用アプリから追加の実モデル送信は行っていない。

Swiftの自動試験は次で実行する。

```sh
swift test --package-path apps/macos
python3 apps/macos/scripts/test_assemble_app.py
```

これらはSwift側のロジックとアプリ組立ての自動試験であり、実画面の操作、実モデル応答、実録音、署名、配布形式を検証するものではない。
