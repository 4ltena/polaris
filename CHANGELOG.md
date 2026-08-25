# 変更履歴

[Keep a Changelog](https://keepachangelog.com/ja/1.1.0/) と [Semantic Versioning](https://semver.org/lang/ja/) に従う。

## [0.5.0] — 2026-08-24 "Regulus"

M4(subagent オーケストレーション基盤)を実装し、これを土台に2つの機能を追加した。会話コンテキストを一切消費せずディレクトリ構成の要約を保守する `files.md` 自動生成と、ツール呼び出し・subagent進捗をターン実行中に逐次表示するライブ表示改良。加えて、TUIの端末統合方式を `codex` の実挙動に合わせてインラインビューポートへ切り替えた。

### 追加

- `spawn` ツール: 1回の呼び出しで複数の subagent を同一波として並列実行する単一波オーケストレーション。書込先(`write_root`)が波内で重複する場合は1件も起動せず波全体を拒否
- `agents/<type>/SKILL.md` による subagent 型定義の frontmatter 解析・discovery(`polaris-access`/`polaris-tier`/`polaris-wall-seconds`/`polaris-max-turns`/`polaris-continuation`/`polaris-output` の各メタデータ)。実例として `file-inspector`(読み取り専用)・`files-md-writer`(読み書き)の2型を同梱
- subagent の結果は宣言済み JSON Schema で検証し、不一致時は検証エラーを添えて1回だけ再試行。壁時計超過・スキーマ不一致2回連続をそれぞれ専用の停止条件として追加
- ディレクトリ別 `files.md` 自動生成: `write`/`edit`/`bash` 実行前後のファイルシステムスナップショット差分から新規ディレクトリ・新規ファイルを検出し、`files-md-writer` subagent を直接起動して該当ディレクトリの `files.md` を最新化する。モデルの判断・会話コンテキストを一切介さず、ハーネス側で決定的に実行
- ツール呼び出し・spawn進捗のライブ表示: `polaris_core::agent::run`/`dispatch` から `AgentEvent`(`ToolStarted`/`ToolFinished`/`SpawnStarted`/`SpawnFinished`)を通知する経路を新設し、TUIがターン実行中にツール名・引数・結果プレビュー、write/editの実差分(40行で打ち切り)、spawnタスクごとの進捗を逐次表示する
- 監査ログに `caller` フィールドを追加し、ルート("root")と各 subagent(型名)の呼び出しを区別して記録

### 変更

- TUIの端末統合方式を、独自のフルスクリーン管理から `ratatui::Viewport::Inline` へ変更し、会話履歴を端末自身のネイティブスクロールバックに一度きり印字する方式に変更(`codex` の実端末挙動を tmux で検証したうえで移植。この方式は v0.6.0 で自前スクロール管理の `Viewport::Fullscreen` へ再度置き換えられた)
- `dispatch` を `async fn` 化(spawn の並列実行を await できるようにする土台)
- `AuditLog`/`Provider` を `Arc` 化し、複数 subagent から共有できるようにした

## [0.4.0] — 2026-08-24 "Acubens"

対話TUIを大幅に拡張した。TUI v2(ステータスバー・ツール呼び出しの可視化・Markdown整形)、初回起動時のオンボーディング画面、codex互換サブコマンド、対話中のスラッシュコマンド群、非同期イベントループ化によるライブなステータス表示を追加した。`codex`の実際のソースコード(GitHubから取得)とデスクトップのスクリーンショットを繰り返し参照し、対応する画面要素の見た目・操作感を検証したうえで移植した。

### 追加

- TUI v2: 画面下部のステータスバー(プロバイダ・モデル・累積トークン使用量)、ツール呼び出しの会話履歴内可視化(`⚙ ツール名(引数)` / `→ 結果`)、`**太字**`・`` `インラインコード` ``・フェンス付きコードブロックの簡易Markdown整形
- オンボーディング画面: プロバイダの資格情報が一切無い状態で対話TUIを起動すると、ChatGPTサインインまたはAPIキー入力を選べる画面を表示する(`POLARIS_PROVIDER`未設定時のみ)。APIキーは`~/.polaris/api_key.json`に0600で保存
- codex互換サブコマンド: `exec`・`sandbox`・`doctor`・`completion`(bash/zsh/fish/powershell/elvish)
- TUIスラッシュコマンド(全16個): `/help`・`/status`・`/skills`・`/new`・`/resume`・`/clear`・`/init`・`/model`・`/diff`・`/review`・`/permissions`・`/fork`・`/export`・`/pwd`・`/logout`・`/quit`
- `/model`: モデル選択→reasoning effort選択の2段階ピッカー(low/medium/high/extra high/max/ultra)。`polaris_provider::Provider`に`set_model`/`set_effort`(内部可変性で実現)を追加し、セッション中に実際に送信モデル・reasoning effortを切り替えられるようにした
- `/resume`: 保存済み会話を起動元ディレクトリ別にグループ化して選べるフルスクリーンピッカー(今いるディレクトリのグループが常に先頭)。会話の永続化先を「プロジェクトごとのディレクトリ」から全プロジェクト共有の`~/.polaris/sessions/`へ移行し、1会話=1ファイルペア(`<id>.jsonl` + `<id>.meta.json`)へ再設計
- `/permissions`・`/skills`: 対話的ピッカー(承認ポリシーの変更、発見済みskillのフルスクリーン一覧)
- ターン実行中のステータス行にshimmerアニメーション付き経過秒表示+esc中断機能。イベントループを非同期化(`tokio::select!`)して実現
- ヘッダーボックス(dimスタイルの枠線+`✦ polaris`見出し+`model:`/`directory:`/`tokens:`各行)、2色構成のフッター(モデル名・reasoning effort・作業ディレクトリの`~`短縮表示)

### 変更

- セッション永続化モデルを「プロジェクトごとに1つの会話を自動再開」から「常に空の状態で起動し`/resume`で選ぶ」方式へ変更
- `/new`の意味を「その場で消去して再開」から「新しい会話へ切り替え、元の会話は保存したまま残す」へ変更(元の会話は引き続き`/resume`から辿れる)
- プロバイダ解決: `POLARIS_PROVIDER`未指定時、保存済みcodex資格情報の有無を見て既定プロバイダを決めるようにした

### 修正

- プロバイダ未指定時、保存済みcodex資格情報があってもデフォルトが常に`openai`に解決されていた不具合
- `/model`のreasoning effortピッカーで表示名(`"extra high"`等、空白入り)をそのままAPIへ送信し実際に`400 Invalid value`を引き起こしていた不具合。APIへ送る値と画面表示名を分離して修正

## [0.3.0] — 2026-08-21 "Castor"

対話TUIを追加した。`polaris` を `--prompt` なしで実行すると、スクロール可能な会話履歴と入力欄を持つ対話セッションに入る。一発実行(`polaris -p "..."`)の挙動は変わらない。

### 追加

- 新規クレート `polaris-tui`。`ratatui` + `crossterm` によるTUI描画・入力処理・承認モーダルを持つ
- セッションの永続化。`~/.polaris/state/<project-id>/tui-session.jsonl` に1メッセージ1行で保存し、次回起動時に再開する。破損行は切り捨てて警告し、それより前の履歴で再開する
- ツール実行承認をTUI内でインタラクティブに行う機能(`y`/`n` のモーダル)

### 変更

- `polaris_core::project` に `project_id` / `state_dir` を公開関数として移動(旧 `polaris-cli` の非公開関数から)

### 修正

- 会話履歴のレンダリングにおけるスクロールオフセットの計算誤り(枠線分の行が考慮されておらず最新メッセージが見切れる不具合)
- 複数行のアシスタント応答が1行に折り畳まれて表示される不具合
- ターン失敗時にツール呼び出しとツール結果の対応が崩れたままセッションが継続し、以降の全ターンが失敗し続ける不具合
- `ratatui::init()` 以降に出力していた診断メッセージ(破損セッション警告・永続化失敗)が画面に一切表示されない不具合
- セッションファイルへの不正UTF-8バイト列でTUIが起動不能になる不具合
- キーリリース/リピートイベントを未フィルタのまま処理し、文字が二重入力されうる不具合

### セキュリティ

- LLM応答・ツール呼び出し理由・エラーメッセージなど外部由来のテキストをそのまま端末へレンダリングしていたことによる、端末エスケープシーケンス注入の可能性を修正。制御文字(ESC・DEL等)をレンダリング前に無害化する

### 備考

- 会話履歴のスクロールバック(過去ログを遡る操作)は未実装。既知の制限として `README.md` に記載した
- 実端末を要する対話的な動作確認(入力・承認モーダル・再開)は自動化していない。手動確認手順を `README.md` に記載した

## [0.2.0] — 2026-08-20 “Aldebaran”

skill/plugin 分類機(M3b)を追加した。BM25ランキングによる skill 検索と、常時コンテキストを skill 数に依存させない near-universal 選定を実装した。

### 追加

- BM25ランキングによる `skill` ツールの検索機能
- near-universal skill の選定(常時コンテキストへの混入を防ぎつつ `lookup` の戻り値にのみ載せる設計)
- 候補一覧の description をプレビュー表示(先頭200バイト)に短縮し、上位8件のみプレビューを残す方式
- codex・pi・polaris 三者の実測比較(830件規模のskill/pluginコーパスでの初回ターン入力トークン数、実行速度)

### 修正

- CJK/部分文字列検索の recall 低下、near-universal がバイト上限で切り詰められる不具合
- `read` ツールの出力バイト上限が `limit` 指定と独立していなかった不具合

## [0.1.0] — 2026-08-20

ヘッドレスの一発実行エージェントとして出荷した。M1(核となるループ)からM3a(skillsローダ)までを含む。

### 追加

- パスポリシー付き `read` ツール、OpenAI互換プロバイダ、エージェントループ、CLI(M1)
- `write` / `edit` / `bash` とサンドボックス、承認境界(M2)
- Codexプロバイダ。`polaris login` によるChatGPTサブスクリプション認証(APIキー不要)(M2.5)
- SKILL.md frontmatterの解析・検証とskillsローダ、`skill` ツール(M3a)
- 伏字化付き追記専用監査ログ、量的な停止条件
- 常時コンテキストを990トークン以下に保つ予算テスト
