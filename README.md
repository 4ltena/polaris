# Polaris

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
  <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — アスタリスクとワードマーク" width="800" height="200">
</picture>

**v0.10.0 “Algedi”** · [変更履歴](CHANGELOG.md)

Polarisは、常時送信する指示とツール定義を小さく保ち、必要な資料だけを検索・再取得するRust製のCLI/TUIエージェントである。会話の圧縮、圧縮前履歴のローカル保存、大きなツール結果の退避と再取得に対応する。

## はじめに

Rust 1.96.0（`rust-toolchain.toml`で固定）を用意してビルドする。

```sh
cargo build --release
POLARIS_API_KEY=sk-... target/release/polaris -p "Cargo.toml は何行か"
```

開発中は `cargo run -p polaris-cli -- -p "..."` でも実行できる。`--prompt`を省略すると対話TUIを起動する。

ChatGPTサブスクリプションで使う場合は、先に認証する。

```sh
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris -p "Cargo.toml は何行か"
```

既定モデルは`gpt-6-astra`、推論強度は`medium`。明示する場合は`POLARIS_MODEL=gpt-6-astra`と`--effort medium`を別々に指定する。

```sh
POLARIS_PROVIDER=codex POLARIS_MODEL=gpt-6-astra target/release/polaris --effort medium -p "Cargo.toml は何行か"
```

プロバイダ、監査ログ、サンドボックス、TUI、保存済み会話、全サブコマンドは[利用ガイド](docs/usage.md)を参照。

## 主な性質

- 常時コンテキストは基準トークナイザー`o200k_base`で990トークン以下、ツールは6個以下に制限し、実際に送るワイヤ形式をテストする。
- skill一覧は常時展開せず、必要なときだけ`skill`で検索する。会話履歴、取得資料、ツール結果、取得したskill本文は固定部分に含めない。
- 大きなツール結果の退避は`--tool-memory off|history|retrieval`で選ぶ。既定は`off`であり、圧縮前履歴を保存する`--remember`とは独立している。
- `retrieval`は保存時点の原文をキーワード検索し、必要な行・段落だけを再取得する。意味検索はローカル埋め込みURLとモデルを明示した場合だけ有効になる。
- `spawn`は独立したsubagentを1波で実行し、型ごとのJSON Schemaで結果を検証する。深さは1に固定する。

固定コンテキストは総入力の上限ではなく、履歴・取得資料・ツール結果は別に加算される。保存結果は取得時点の原文であり、現在のファイル内容が必要な場合は通常のパスを読む。

ツール結果の保存、検索URI、圧縮、測定用の実行方法は[コンテキスト効率化](docs/context-efficiency.md)、subagentの設定と制約は[subagent](docs/subagents.md)にある。

## v0.10.0の変更

- 保存済みツール結果の記録内検索、filemapの案内、隔離環境での通信と認証に対応した。
- 既定モデルを`gpt-6-astra`、推論強度を`medium`に統一した。
- 本文量を変えずにキャッシュを改善する任意設定と、長期対話・skill数・tool数・例示数を含む品質検証を追加した。`POLARIS_CACHE_PACING=on`は要求開始を最低5秒間隔にする。既定は`off`。

36ターンの完了した1組では、同じ総トークン数でキャッシュ率が54.11%から63.83%へ上がり、API換算費用が15.38%下がった。ただし別課題の対照側が品質不合格で停止したため、全体の改善や再現性は未確認である。[条件・結果・制約](docs/gpt6-cache-pacing-results.md)を参照。

## 文書

| 文書 | 内容 |
| --- | --- |
| [利用ガイド](docs/usage.md) | ビルド、認証、監査ログ、TUI、会話、スラッシュコマンド。 |
| [コンテキスト効率化](docs/context-efficiency.md) | 圧縮、原文保存、検索、埋め込み、測定手順。 |
| [subagent](docs/subagents.md) | `spawn`の設定、型、出力Schema、実行制限。 |
| [テスト](docs/testing.md) | 固定コンテキストの検査とTUIの手動確認。 |
| [skill/plugin大量時の比較](docs/skill-scaling-benchmark.md) | 旧来の初回入力と時間の測定条件・結果。 |
| [GPT-6 medium実測結果](docs/gpt6-efficiency-results.md) | 段階別の合成課題、通常Codex比較、測定上の制約。 |
| [キャッシュと品質検証](docs/gpt6-cache-affinity-results.md) | 本文量を維持する通信変更、長期対話、skill・tool数と例示数の検証。 |
| [要求間隔とキャッシュ](docs/gpt6-cache-pacing-results.md) | 本文量を変えずに送信間隔だけを変える実験。 |

## 通常Codexとの比較

v0.9.0時点で、通常Codex CLIとPolaris候補版を、同じ合成資料・依頼文、GPT-6 medium、各3回で比較した。通常Codex側は測定時のユーザー設定・スキルを維持している。

| 指標（各3回合計） | 通常Codex | Polaris | Polarisの削減率 |
| --- | ---: | ---: | ---: |
| 総トークン | 612,027 | 38,620 | 93.7% |
| 処理時間 | 93.678秒 | 72.096秒 | 23.0% |
| 正答・変更範囲 | 3/3合格 | 3/3合格 | — |

総トークンはキャッシュ済み入力を含む入力と出力の合計で、料金比較ではない。単一の合成課題・別時刻の測定であり、v0.10.0全体や一般の実装作業での削減率を示さない。[詳細と制約](docs/gpt6-efficiency-results.md)を参照。

以前の大量skill/plugin比較では、Polarisの初回入力は547トークンだった。この値は当時のAPI使用量であり、現在の固定コンテキストの測定値ではない。測定条件と全表は[skill/plugin大量時の比較](docs/skill-scaling-benchmark.md)にある。

## 開発と検証

```sh
cargo fmt --all -- --check
cargo test --locked --offline -p polaris-core budget
cargo test --locked --offline -p polaris-cli tool_memory_defaults_off_and_is_independent_of_remember
```

検査の対象、TUIの手動確認、環境依存の制限は[テスト](docs/testing.md)を参照。

## ライセンス

[MIT License](LICENSE)で公開する。TLSテスト素材の出典と許諾は[fixtureの説明](crates/polaris-http/src/fixtures/README.md)を参照。
