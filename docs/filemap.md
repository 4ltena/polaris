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
- `rust-toolchain.toml` — ツールチェイン固定

## `crates/polaris-core`

- `Cargo.toml` — polaris-core クレートのマニフェスト

## `crates/polaris-core/src`

- `audit.rs` — 追記専用の監査ログ。署名は付けない。インプロセスでは署名する主体と
- `budget.rs` — 常時コンテキストの計測。数値は測定で担保し、見積で運用しない。
- `constitution.rs` — 常時載る文脈のうち、ハーネスが所有しない部分。AGENTS.md の全文は載せない。
- `lib.rs` — polaris-core の入口。予算・憲法・プロンプトの各モジュールを束ねる。
- `prompt.rs` — 毎ターン送るシステムプロンプトを定義し、憲法と環境情報を差し込んで組み立てる。

## `crates/polaris-core/src/secret_screen`

- `mod.rs` — Remna のプライバシーフィルタ。捕捉したイベント（コマンド文字列やウィンドウ

## `crates/polaris-core/tests`

- `filemap.rs` — `docs/filemap.md` がリポジトリの実体と一致しているかを確かめるスナップショットテスト。

## `crates/polaris-tools`

- `Cargo.toml` — polaris-tools クレートのマニフェスト

## `crates/polaris-tools/src`

- `lib.rs` — polaris の組込みツール。常時提供するツールは 6 本を超えない。
- `path_policy.rs` — 読み取りを拒否するパスの判定。過検出より見逃しを避ける方向に倒す。
- `read.rs` — read ツール。行番号を付けて返すのは、モデルが path:line で位置を示せるようにするため。

## `docs`

- `filemap.md` — ファイルマップ

## `docs/superpowers`

- `CURRENT.md` — polaris 現況

## `docs/superpowers/plans`

- `2026-08-16-polaris-m1-headless-loop.md` — polaris M1 ヘッドレス最小ループ Implementation Plan

## `docs/superpowers/specs`

- `2026-08-16-polaris-harness-design.md` — polaris 設計仕様
