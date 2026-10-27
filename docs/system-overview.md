# システム構成図

Polarisの現行ソースを、利用者の入力から実行・保存までの流れで整理する。v1.0.0-rcの配布対象はmacOS arm64のTUIである。GUIと他OSのソースは保持するが、この配布の受入対象には含めない。

## 全体構成

```mermaid
flowchart TB
  user[利用者] --> cli[polaris-cli：起動・認証選択・設定・入力受理]
  cli --> tui[polaris-tui：入力・補完・表示・承認・管理コマンド]
  tui --> core[polaris-core：セッションとagent loop]
  core --> context[workflow・skill解決・履歴検索・要求組立]
  context --> provider[polaris-provider：主モデル・役割別モデル]
  provider --> cloud[Codex / OpenAI互換 endpoint]
  provider --> local[Ollama / LM Studio / llama.cpp]
  provider --> http[polaris-http：HTTP境界]
  cli --> envowner[core environment owner：C43・project lease・計画承認]
  tui --> envowner
  core --> envowner
  envowner --> envkit[polaris-envkit：固定profile・互換候補・private runtime]
  envkit --> artifacts[credential-free HTTPS：固定size/hash・verified-chain鍵拒否]
  envkit --> offline[offline process：有限時間・出力・disk上限]
  offline --> inventory[postimage inventory・create-only CAS・path receipt]
  inventory --> workspace
  envowner --> store
  cli --> auth[polaris-auth：アカウント・認証保存]
  provider --> auth
  core --> tools[polaris-tools：read / write / edit / bash / skill / spawn]
  tools --> sandbox[polaris-sandbox：隔離・許可・helper実行]
  sandbox --> workspace[利用者の作業ファイル]
  core --> mcp[polaris-mcp：登録・catalog・選別・呼出]
  mcp --> servers[明示登録されたMCP server]
  core --> store[会話・世代・巻戻し・audit・復旧記録]
  core --> memory[polaris-memory：原文参照・検索・索引]
  store --> disk[プロジェクト外の保存領域]
  memory --> disk
  context --> skills[polaris-skills：skill / agent定義の発見]
  core --> children[子agent：role・tier・attempt・停止要求]
  children --> provider
  children --> tools
  core -. 任意の型付き判断 .-> judge[Jev / SemIf / System One互換 / Clef adapter]
  gui[SwiftUI GUI：今回は配布対象外] -. IPC .-> service[desktop-service / desktop-protocol]
  service -. 共通実装を使用 .-> core
```

矢印は要求・データの流れを表し、すべてが別processという意味ではない。crate間の正確な依存と外部接続箇所は、ソースから生成する[アーキテクチャ一覧](architecture.md)を参照。

## 1回の入力から応答まで

```mermaid
sequenceDiagram
  participant U as 利用者
  participant T as CLI/TUI
  participant C as core
  participant P as provider
  participant X as tools/MCP/子agent
  participant S as 保存owner
  U->>T: 入力・Enter
  T->>T: 入力受理・受理ID表示
  T->>C: run開始／待機入力の適用
  C->>S: セッション世代・実行状態を確認
  C->>C: workflow・必要文脈・権限を組立
  C->>P: モデル要求
  P-->>C: 本文／tool calls／usage
  opt tool callがある
    C->>C: 承認・sandbox・実行上限を確認
    C->>X: 実行
    X-->>C: 結果・失敗・取消状態
    C->>S: 原文・結果・auditを記録
    C->>P: tool結果を含む次要求
    P-->>C: 応答
  end
  C->>S: 完了状態と会話を保存
  C-->>T: 描画用イベントと応答
  T-->>U: 本文・状態・使用量を表示
```

中断や通信失敗で外部結果が不明な場合は、その状態を表示する。停止要求の受付や`outcome_unknown`を、安全な停止・外部操作の取消完了とは扱わない。

## コンポーネントと実装箇所

| 機能 | 主な実装 | 役割 |
| --- | --- | --- |
| 起動とprovider準備 | `crates/polaris-cli/src/main.rs`、`provider_startup.rs` | 引数・環境設定、認証選択、local serverの寿命、TUI起動 |
| TUI | `crates/polaris-tui/src/lib.rs`、`slash.rs` | 入力編集・補完、working表示、管理コマンド、確認画面 |
| agentと文脈 | `crates/polaris-core/src/agent.rs`、`prompt.rs`、`workflow.rs` | モデルとtoolの往復、段階・skill・履歴の組立 |
| 環境構築 | `crates/polaris-core/src/environment/`、`crates/polaris-envkit/` | CLI/TUI/tool共通の計画・固定取得・offline実行・非上書きpublish・照合復旧 |
| モデル接続 | `crates/polaris-provider/src/`、`crates/polaris-http/src/` | Codex・OpenAI互換・ローカル推論、通信とusage |
| 認証と保存 | `crates/polaris-auth/src/`、coreの`session*`・`desktop_store/` | アカウント、会話・世代・保存結果の照合 |
| toolと隔離 | `crates/polaris-tools/src/`、`crates/polaris-sandbox/src/` | ファイル・shell操作、実行helper、承認境界 |
| MCP | `crates/polaris-mcp/src/`、coreの`mcp/` | stdio/HTTP接続、catalog、候補選別、結果保持 |
| skillと子agent | `crates/polaris-skills/src/`、coreの`spawn.rs`・`tier/` | 定義発見、role/tier、子実行の管理 |
| 原文と検索 | `crates/polaris-memory/src/`、coreの`conversation_memory/` | 原文参照、検索・索引、任意のembedding接続 |
| 任意判断器 | coreの`decision_bands.rs`・`clef.rs`・`managed_decision.rs` | backendの明示選択、回答検証、容量判定、管理generation |
| GUI経路 | `apps/macos/`、`polaris-desktop-service`、`polaris-desktop-protocol` | 共通coreに接続する別の操作画面。今回の配布対象外 |

## 配布範囲と利用条件

| 項目 | v1.0.0-rcの扱い |
| --- | --- |
| macOS arm64 TUI | 配布対象。`polaris`で起動する |
| CLI管理・一発実行 | 同じ実行物の入口として保持する |
| GUI・Windows・Linux | ソースを保持し、今回の配布・実機受入の対象から除外する |
| 主モデル | 利用者が認証またはlocal endpointを設定する。モデルweightsは同梱しない |
| MCP | 登録・許可が必要。すべての外部serverの動作確認を意味しない |
| Jev/SemIf/System One互換 | 任意設定を保持。導入だけで有効化・自動転送しない |
| Clef/Clef-flash | adapterと管理経路はあるが、本番受入profileは0件。通常利用は未提供 |
| strict履歴・embedding | 資源・設定と個別条件が必要。既定の通常会話と区別する |

型付き判断と生成モデルの仲裁は別の経路である。明示的なlocal-onlyは補助判断のremote送信を抑止する。主会話の接続許可やMCPの許可を、この設定で変更するものではない。

詳しい操作は[コマンド参照](command-reference.md)、利用条件は[利用ガイド](usage.md)、公開範囲と検証結果は[v1.0.0-rcリリース資料](releases/v1.0.0.md)を参照。
