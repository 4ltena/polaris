# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — アスタリスクとワードマーク" width="800" height="200">
  </picture>
</p>

<p align="center">
  <strong>暫定v1.0.0 · macOS TUI限定 · 公開準備中</strong> · <a href="CHANGELOG.md">変更履歴</a>
</p>

<p align="center">
  <strong>日本語</strong> · <a href="README.en.md" lang="en">English</a>
</p>

Polarisは、作業に必要な文脈を選んでモデルへ渡すRust製のコーディングエージェントである。CLIとTUIを主な操作経路とし、macOS向けの作業画面も備える。workflowの段階を状態として保存し、その段階に必要な指示・skillを組み立てる。

暫定v1.0.0はmacOS arm64のTUIを配布対象とする。GUI・他OSのソースは保持するが、この版の配布対象には含めない。会話・巻戻し・MCP・subagentの操作は[利用ガイド](docs/usage.md)と[コマンド参照](docs/command-reference.md)、構成と今回の提供範囲は[システム構成図](docs/system-overview.md)と[リリース資料](docs/releases/v1.0.0.md)を参照。現在は公開準備中である。

## 暫定版の利用方法

起動前に作業対象のプロジェクトへ移動する。ホームディレクトリからの起動は、現在`RuntimeDenied`で停止する。[起動時のトラブル](docs/usage.md#起動時のトラブル)を参照。

macOS arm64では、ビルド済みCLIを同梱したローカル検証用パッケージを作成できる。パッケージを取得した後の導入は`sudo installer -pkg polaris-cli-preview-1.0.0-arm64.pkg -target /`の1コマンドで、以後は`polaris`で起動する。利用者側にRustは不要。現時点では未署名・未公開であり、実機へのインストール受入も未完了。組立て方法と検証範囲は[macOSの説明](apps/macos/README.md)を参照。

このcheckoutをソースから実行する場合はRust 1.96.0を用意する。ChatGPT認証を使う例は次のとおり。

```sh
cargo build --release -p polaris-cli
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris
```

引数を省略すると対話TUI、`-p`を指定すると一発実行になる。プロバイダ未指定時は、OpenAI互換APIキーがあれば`openai`、APIキーがなく保存済みChatGPT認証があれば`codex`を選ぶ。いずれもなければ対話起動時にログイン画面を開く。例では`codex`を明示している。ソースから試す場合は`target/release/polaris`を指定し、導入済みの別版と区別する。

段階を指定して始める場合は、`--phase`を使う。

```sh
POLARIS_PROVIDER=codex target/release/polaris --phase specify -p "機材貸出サービスの受入条件を整理する"
```

APIキー用の`openai`プロバイダはChat Completions形式のため、Responses APIが必要なAstraのtool callingには未対応である。認証・設定・会話の再開方法は[利用ガイド](docs/usage.md)にまとめる。

## 主な機能

- **状態で管理するworkflow**：段階、作業、必要なskillを記録し、毎ターンの文脈を組み立てる。
- **会話の保存と分岐**：Sidechatで別文脈を分け、保存済み会話を再開・分岐できる。TUIでは対象のユーザー入力より前へ会話と作業ファイルを巻き戻せる。
- **ツールと委譲**：ファイル・Git・MCPの操作とsubagentへの委譲を共通の実行基盤で扱う。MCPの接続条件と操作権限を検査する。
- **記憶と取得**：原文を保持し、直近のターンと過去の関連情報を必要に応じて取り出す。大きなツール結果は参照から再取得できる。
- **選べる型付き判断器**：Jev・SemIfを維持し、Clef/Clef-flashの固定profileとSystem One互換接続を共通の判断契約で扱う。既定は無効。容量帯は推薦の目安で、backendを自動変更しない。Clefの本番受入profileはまだ0件であり、通常実行は未提供である。[設定と現在の制約](docs/usage.md#型付き判断器の設定)を参照。

macOSのCLI専用プレビューパッケージには追加資源を含めない。GUIインストーラには固定manifestからllama.cpp、埋め込み実行環境、multilingual-e5-small/baseを検証して`~/.polaris`へ取得する経路があり、Rustは任意選択である。Qwen系はSemIf用のQwen3.5-4Bを候補としているが、固定資源の登録と受入が未完了で、自動取得しない。署名済み配布資源と実機受入も未完了である。検証済み範囲は[macOSデスクトップ](apps/macos/README.md)に記載する。

## v0.12.0での純正Codexとの比較

これはv0.12.0の過去の測定であり、暫定v1.0.0の実測結果ではない。現行版の再比較は未実施である。

同じ`gpt-6-astra`／`medium`で、workflowを有効にしたPolarisとCodex CLI 0.154.0を比較した。下表はPRE02〜08で課題・反復がそろった15試行、各2ターンの部分結果である。PRE01の追加測定は省略した。

| 指標 | Polaris | 純正Codex CLI 0.154.0 | Polarisの削減率 |
| --- | ---: | ---: | ---: |
| 総トークン | **95,139** | 467,901 | **79.7%** |
| API換算費用 | **$2.37915** | $3.600914 | **33.9%** |
| 品質検査 | 15/15合格 | 15/15合格 | — |

総トークンはキャッシュ済み入力を含む入力と出力の合計。費用は測定仕様で固定したAPI単価による参考額で、Codexの実請求額ではない。この15組ではPolarisの所要時間は23.5%長かった。短い合成課題による逐次測定であり、一般の実装作業や長期対話へは外挿しない。[測定条件・品質評価・時間・制約](docs/preimplementation-evaluation.md#v0120とcodex-01540の途中結果)を参照。

## 文書

目的別の入口は[文書一覧](docs/README.md)を参照。以下の実測文書は対象版と測定条件を確認して読む。

| 文書 | 内容 |
| --- | --- |
| [文書一覧](docs/README.md) | 現行ガイド、実装資料、過去の測定を目的別に整理。 |
| [利用ガイド](docs/usage.md) | 認証、起動トラブル、設定、envkit、会話操作。 |
| [コマンド参照](docs/command-reference.md) | CLI/TUIコマンドの役割と出力。 |
| [システム構成図](docs/system-overview.md) | 実行、保存、隔離、各crateの役割。 |
| [暫定v1.0.0](docs/releases/v1.0.0.md) | 配布範囲、候補checksum、検証結果と限界。 |


## 開発と検証

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## ライセンス

[MIT License](LICENSE)で公開する。TLSテスト素材の出典と許諾は[fixtureの説明](crates/polaris-http/src/fixtures/README.md)を参照。
