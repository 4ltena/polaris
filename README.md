# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — アスタリスクとワードマーク" width="800" height="200">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/4ltena/polaris/releases/tag/v0.12.0"><strong>v0.12.0 “Alrescha”</strong></a> · <a href="CHANGELOG.md">変更履歴</a>
</p>

<p align="center">
  <strong>日本語</strong> · <a href="README.en.md" lang="en">English</a>
</p>

Polarisは、macOSの作業画面とCLI/TUIを備えたRust製のコーディングエージェントである。必要な指示と資料だけをモデルへ渡し、作業段階を記録するworkflow、隔離実行、会話の保存・検索を組み合わせる。

v0.12.0 “Alrescha”は[macOS用DMGとソースコード](https://github.com/4ltena/polaris/releases/tag/v0.12.0)を配布する。macOSの画面には、実モデル接続、ファイル・Git・作業表示、ローカルモデル、復旧、`strict10`の会話記憶を統合した。

## 純正Codexとの比較

2026-09-10に同じ`gpt-6-astra`／`medium`で、workflowを有効にしたPolarisとCodex CLI 0.154.0を比較した。下表はPRE02〜08で課題・反復がそろった15試行、各2ターンの部分結果である。PRE01の追加測定は省略した。

| 指標 | Polaris | 純正Codex CLI 0.154.0 | Polarisの増減 |
| --- | ---: | ---: | ---: |
| 総トークン | **95,139** | 467,901 | **79.7%減** |
| API換算費用 | **$2.37915** | $3.600914 | **33.9%減** |
| ターン時間の合計 | 1,179.26秒 | 954.94秒 | 23.5%増 |
| 品質検査 | 15/15合格 | 15/15合格 | — |

総トークンはキャッシュ済み入力を含む入力と出力の合計。費用は測定仕様で固定したAPI単価による参考額で、Codexの実請求額ではない。短い合成課題による逐次測定であり、一般の実装作業や長期対話へは外挿しない。[測定条件・品質評価・制約](docs/preimplementation-evaluation.md#v0120とcodex-01540の途中結果)を参照。

## はじめに

### macOSアプリ

| Download | Architecture | Requirements |
| --- | --- | --- |
| [polaris-0.12.0-arm64.dmg](https://github.com/4ltena/polaris/releases/download/v0.12.0/polaris-0.12.0-arm64.dmg) | Apple Silicon (arm64) | macOS 13.5 or later |
| [SHA-256 checksums](https://github.com/4ltena/polaris/releases/download/v0.12.0/polaris-0.12.0-SHA256SUMS.txt) | — | Verify the downloaded DMG |

DMGを開き、宇宙側のPolarisを地球側のApplicationsへドラッグする。コピーが終わったらApplications内のPolarisを開き、6ページの初回設定を進める。DMGから直接起動しない。

Developer ID署名・公証はない。開発元を確認できないと表示された場合の扱いは[Appleの案内](https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unknown-developer-mh40616/mac)を参照。アプリと実行helper、Node.js 24.11.1を同梱する。npmや他言語のビルド環境、`strict10`の埋め込み資源は別途必要である。[初回設定・同梱範囲](apps/macos/README.md#配布パッケージ)に詳細を記載する。

### ソースからCLIを使う

Rust 1.96.0とGitを用意する。v0.12.0のソースを取得してCLIをビルドし、ChatGPTサブスクリプションで認証する。

```sh
git clone --branch v0.12.0 --depth 1 https://github.com/4ltena/polaris.git
cd polaris
cargo build --locked --release -p polaris-cli
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris -p "Cargo.toml は何行か"
```

`-p`を省略すると対話TUIを起動する。開発中は`cargo run -p polaris-cli -- -p "..."`でも実行できる。既定モデルは`gpt-6-astra`、推論強度は`medium`である。

段階を指定して始める場合は、`--phase`を使う。

```sh
POLARIS_PROVIDER=codex target/release/polaris --phase specify -p "機材貸出サービスの受入条件を整理する"
```

APIキー用の`openai`プロバイダはChat Completions形式のため、Responses APIが必要なAstraのtool callingには未対応である。認証・設定・保存済み会話の再開方法は[利用ガイド](docs/usage.md)にまとめる。

macOSの画面をソースから組み立てる場合は、macOS 13以降とSwift 6を使う。[macOSガイド](apps/macos/README.md#組立て)に手順を記載する。

## 主な機能

- **macOSの作業画面**：会話、ファイル、Git、実行状態を表示し、確認済みテキスト添付、モデル・権限設定、Ollama・LM Studioの接続を扱う。モデルの編集は隔離領域へ保存し、元ファイルへの反映を別途確認する。
- **段階別workflow**：共通skillと現在の段階の必須skillを各ターンで固定する。通常のskill検索は必要なときに使い、カタログ全体を常時展開しない。
- **小さな基底コンテキスト**：基底の指示と最大6個のツール定義は`o200k_base`で990トークン以下。workflowのskillと識別情報には別枠で最大384トークンを設ける。履歴・取得資料・ツール結果はこの固定部分に含めない。
- **保存と再取得**：大きなツール結果を原文とともに保存し、必要な行や段落だけを取り出せる。ツール記憶は`--tool-memory history|retrieval`で有効化する。
- **会話の継続**：workflowの段階と会話を保存し、再開・分岐で引き継ぐ。子エージェントは1波で並列実行し、型ごとの出力Schemaで検証する。
- **strict10の記憶**：GUIの履歴設定から選択できる。全原文を保存し、直接投入は直近10ターン、それ以前は要約とローカル埋め込みから検索する。固定埋め込み資源を別途設定する。

## 検証状況

v0.12.0のソースではRust 1,689件、Swift 236件、梱包試験6件が成功し、Clippy・fmt・filemapも確認した。個人設定や既存Swiftビルドキャッシュのない環境で、macOSアプリの組立ても成功している。

配布準備ではrelease構成を含む梱包試験7件、DMGの整合性、533ファイルの内容と展開後の所有者・権限、Finderの上下配置を確認した。配布用アプリの実モデル再送は行っていない。

`strict10`の検索不具合は修正済みで、自動回帰と履歴復元を確認した。修正後の実モデル追加確認と比較のPRE01再測定は省略している。音声入力は合成試験のみで、Windows・Linuxの実機確認は対象外。Web検索は現行CLIでは有効化できない。詳細は[macOSの検査](apps/macos/README.md#検査)と[変更履歴](CHANGELOG.md)を参照。

## 文書

| 文書 | 内容 |
| --- | --- |
| [利用ガイド](docs/usage.md) | 認証、設定、workflow、TUI、会話の再開・分岐。 |
| [macOSデスクトップ](apps/macos/README.md) | アプリの組立て、保存境界、ローカルモデル、strict10。 |
| [コンテキスト効率化](docs/context-efficiency.md) | 圧縮、原文保存、検索、埋め込み、strict10。 |
| [subagent](docs/subagents.md) | 並列実行、出力Schema、実行制限。 |
| [実装前段階の実測](docs/preimplementation-evaluation.md) | 企画から計画レビューまでの品質と純正Codex比較。 |
| [テスト](docs/testing.md) | 自動検査とTUIの手動確認。 |

過去版の検証と実測は[変更履歴](CHANGELOG.md)から参照できる。

## 開発と検証

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## ライセンス

[MIT License](LICENSE)で公開する。TLSテスト素材の出典と許諾は[fixtureの説明](crates/polaris-http/src/fixtures/README.md)を参照。
