# ファイルマップ

生成ファイルである。手で編集しない。`crates/polaris-core/tests/filemap.rs` が
`git ls-files --cached --others --exclude-standard` の結果からリポジトリの実体を
読み、本文を再構築して `docs/filemap.md` と突き合わせる。ずれていればテストが
失敗する。更新するときは次を実行する。

```
UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap
```

## `.`

- `Cargo.toml` — ワークスペース定義と共有依存
- `README.md` — polaris
- `rust-toolchain.toml` — ツールチェイン固定

## `crates/polaris-auth`

- `Cargo.toml` — polaris-auth クレートのマニフェスト

## `crates/polaris-auth/src`

- `lib.rs` — ChatGPT のサブスクリプション認証（OAuth）の寿命と資格情報の保管。
- `login.rs` — 認可 URL の組み立てと、コールバックの一度きりの受け取り。
- `pkce.rs` — PKCE (RFC 7636) の verifier と challenge。
- `store.rs` — 資格情報の保管。`~/.polaris/auth.json`、0600、原子的書き込み。
- `token.rs` — `/oauth/token` への交換と更新。

## `crates/polaris-cli`

- `Cargo.toml` — polaris-cli クレートのマニフェスト

## `crates/polaris-cli/src`

- `main.rs` — `polaris` バイナリの入口。環境変数から接続先を決め、常時コンテキストを

## `crates/polaris-cli/tests`

- `cli.rs` — CLI バイナリの統合テスト。実プロセスを起動して振る舞いを確かめる。
- `confined_helper.rs` — 本物の `polaris` バイナリを、本物の拘束プロファイルの下でヘルパとして
- `subcommands.rs` — サブコマンドが実際に到達することを固定する。

## `crates/polaris-core`

- `Cargo.toml` — polaris-core クレートのマニフェスト

## `crates/polaris-core/src`

- `agent.rs` — エージェントループ。ツール呼び出しが無くなった時点の本文を返す。
- `approval.rs` — 承認境界。`sandbox_mode` が技術的境界を、`approval_policy` が停止して
- `audit.rs` — 追記専用の監査ログ。署名は付けない。インプロセスでは署名する主体と
- `budget.rs` — 常時コンテキストの計測。数値は測定で担保し、見積で運用しない。
- `config.rs` — 設定ファイルの読み込み。存在しないことは正常だが、壊れていることは正常ではない。
- `constitution.rs` — 常時載る文脈のうち、ハーネスが所有しない部分。AGENTS.md の全文は載せない。
- `lib.rs` — polaris-core の入口。予算・憲法・プロンプトの各モジュールを束ねる。
- `project.rs` — プロジェクトルートの解決。
- `prompt.rs` — 毎ターン載るもの一式を組み立てる唯一の場所。
- `session.rs` — メッセージ履歴。M1 では追加のみで、圧縮もディスクへの永続化も持たない。
- `stop.rs` — 停止条件。自動修復は行わない。壊れたまま回り続けるのが最も高くつくため、

## `crates/polaris-core/src/secret_screen`

- `mod.rs` — Remna のプライバシーフィルタ。捕捉したイベント（コマンド文字列やウィンドウ

## `crates/polaris-core/tests`

- `filemap.rs` — `docs/filemap.md` がリポジトリの実体と一致しているかを確かめるスナップショットテスト。

## `crates/polaris-provider`

- `Cargo.toml` — polaris-provider クレートのマニフェスト

## `crates/polaris-provider/src`

- `codex.rs` — ChatGPT のサブスクリプション認証で Responses API を話すプロバイダ。
- `lib.rs` — プロバイダ抽象。トランスポートに依存する部分は各実装が持ち、
- `openai.rs` — OpenAI 互換のチャット補完。base_url を差し替えれば互換エンドポイントも叩ける。
- `sse.rs` — SSE(text/event-stream)を逐次デコードする。バイト片を push すると、

## `crates/polaris-sandbox`

- `Cargo.toml` — polaris-sandbox クレートのマニフェスト

## `crates/polaris-sandbox/src`

- `confine.rs` — 拘束下でのプロセス起動。プラットフォームごとの実装をここで振り分ける。
- `helper.rs` — 拘束された子の中で実行する変更操作。
- `lib.rs` — サンドボックス方針の定義と、OS 機構への委譲。
- `linux.rs` — Linux の強制。landlock の ruleset を子の中で自分自身へ適用する。
- `macos.rs` — macOS の強制。実行時に Seatbelt プロファイルを組み立て、
- `policy.rs` — 方針と書込可能ルート。ルートは構築時に正規化する。
- `stage.rs` — ヘルパ用バイナリを書込可能ルートの外へ退避する。

## `crates/polaris-skills`

- `Cargo.toml` — polaris-skills クレートのマニフェスト

## `crates/polaris-skills/src`

- `discovery.rs` — skill の探索。1 件の破損が全体を巻き込まないよう、読めないものは飛ばす。
- `frontmatter.rs` — SKILL.md のフロントマター解析。仕様が定める制約だけを検証し、独自の制約を足さない。
- `lib.rs` — Agent Skills 仕様に準拠した skill の読み込み。独自のフロントマターは足さない。

## `crates/polaris-tools`

- `Cargo.toml` — polaris-tools クレートのマニフェスト

## `crates/polaris-tools/src`

- `bash.rs` — `bash` tool. Runs a command inside a confined `/bin/sh`.
- `edit.rs` — `edit` tool. Goes through the same confined path as `write`.
- `lib.rs` — polaris's built-in tools. The always-on tool set never exceeds 6 tools.
- `path_policy.rs` — Decides which paths reading is denied on. Leans toward avoiding missed
- `predicate.rs` — Predicts ahead of time whether a write will be denied.
- `read.rs` — read tool. Line numbers are attached to the output so the model can
- `skill.rs` — skill tool. Returns the body on an exact name match, otherwise returns a
- `write.rs` — `write` tool. The actual write happens inside a confined child process.

## `docs`

- `filemap.md` — ファイルマップ

## `docs/superpowers`

- `CURRENT.md` — polaris 現況

## `docs/superpowers/plans`

- `2026-08-16-polaris-m1-headless-loop.md` — polaris M1 ヘッドレス最小ループ Implementation Plan
- `2026-08-17-polaris-m2-write-and-sandbox.md` — polaris M2 実装計画 — write / edit / bash とサンドボックス
- `2026-08-17-polaris-m3a-skills.md` — polaris M3a Skills ローダと skill ツール Implementation Plan
- `2026-08-18-polaris-m25-codex-provider.md` — polaris M2.5 Codex プロバイダ 実装計画

## `docs/superpowers/specs`

- `2026-08-16-polaris-harness-design.md` — polaris 設計仕様
- `2026-08-18-polaris-codex-provider-design.md` — polaris Codex プロバイダ 設計
