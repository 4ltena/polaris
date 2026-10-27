# 文書一覧

v1.0.0-rcの配布対象はmacOS arm64のCLI/TUI。RCパッケージは未公開であり、GUI・他OSのソースがあることと、その配布・実機受入が済んでいることは区別する。

## 現行版を使う

| 目的 | 文書 |
| --- | --- |
| 概要とソースからの起動 | [日本語README](../README.md)・[English README](../README.en.md) |
| 配布範囲、候補checksum、検証結果、未完了事項 | [v1.0.0-rcリリース資料](releases/v1.0.0.md) |
| 認証、起動エラー、モデル、envkit、会話操作 | [利用ガイド](usage.md) |
| CLI/TUIコマンドの役割と出力 | [コマンド参照](command-reference.md) |
| 通常の検査と手動確認 | [テスト](testing.md) |

コマンドの仕様はコマンド参照、検証の結果と限界はリリース資料へまとめる。文書だけの更新は、バイナリを再ビルド・再検証したことを意味しない。

## 実装を調べる

| 目的 | 文書 |
| --- | --- |
| 入力から実行・保存までの構成 | [システム構成図](system-overview.md) |
| crate依存と外部接続点（ソースから生成） | [アーキテクチャ](architecture.md) |
| ソースファイル一覧（ソースから生成） | [filemap](filemap.md) |
| 原文保存、検索、圧縮、埋め込み | [コンテキスト効率化](context-efficiency.md) |
| subagentの制限と出力 | [subagent](subagents.md) |
| ツール候補の選別 | [tool preselection](tool-preselection.md) |
| macOSアプリの組立てと保存境界 | [macOSの説明](../apps/macos/README.md)（GUIは今回の配布対象外） |

## 過去の測定・評価

次は記載された版・モデル・試行条件での記録であり、v1.0.0-rcの性能やキャッシュ率を示すものではない。現行版とCodexの再比較は未実施。数値の条件・除外・未完了範囲は各文書を参照する。

| 文書 | 内容 |
| --- | --- |
| [実装前段階の評価](preimplementation-evaluation.md) | v0.11.0の評価計画とv0.12.0/Codex 0.154.0の部分結果 |
| [v0.11.0の検証](sadalmelik-validation.md) | 制御試験、実モデル試験、未検証範囲 |
| [GPT-6効率測定](gpt6-efficiency-results.md) | v0.9.0の合成課題 |
| [キャッシュ率と費用](gpt6-cache-cost-results.md) | v0.10.0開発中の設定比較 |
| [要求間隔とキャッシュ](gpt6-cache-pacing-results.md) | 長期対話の要求間隔実験 |
| [キャッシュaffinity](gpt6-cache-affinity-results.md) | キャッシュ経路の比較 |
| [skill/plugin大量時](skill-scaling-benchmark.md) | 初回入力とskill数の測定 |
| [Codex更新調査](codex-update-efficiency.md) | 公開変更と自動モデル要求の調査 |

変更の経緯は[変更履歴](../CHANGELOG.md)を参照。古い測定・検証記録は削除せず保持する。
