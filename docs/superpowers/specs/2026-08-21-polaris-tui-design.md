# polaris TUI 設計

日付: 2026-08-21
ステータス: ドラフト(ユーザーレビュー待ち)

## 背景

polaris は M1 で「ヘッドレスの一発実行」エージェントとして出荷された。`docs/superpowers/CURRENT.md` のロードマップ表は M5 を「TUI、圧縮、マルチプロバイダ、セッション永続化」と定義しており、対話TUIはもともと計画済みの領域である。本設計はその前倒し着手にあたる。現在進行中の M4(subagent・単一波オーケストレーション、`worktree-feat-m4-core` で作業中)とは独立した機能であり、M4 の完了を待つ必要はない。

`crates/polaris-core/src/session.rs` の `Session` は既に複数ターンの履歴を保持できる構造(`push_user` / `push_assistant` / `push_assistant_tool_calls` / `push_tool_result`)を持つが、ファイル先頭のコメントに「Persistence and resume land in M5」と明記されている通り、ディスクへの永続化はまだ無い。`polaris-cli/src/main.rs` は `--prompt` を1回送って結果を返し終了する一発実行のみをサポートする。

### 参考にした先行実装と、そこから外した理由

`openai/codex` の `codex-rs/tui` クレート(Apache-2.0、こちらの `MIT OR Apache-2.0` デュアルライセンスと両立可)を調査した。`ratatui` + `crossterm` という技術選定、チャット履歴スクロール + 入力欄という画面構成、承認待ちのインラインプロンプトという操作感は踏襲する価値があると判断した。

一方で `codex-rs/tui` は 15MB・約269,000行、`Cargo.toml` の依存に `codex-` prefix の内部クレートが45個(`codex-config`、`codex-protocol`、`codex-rollout`、`codex-app-server-*`、`codex-plugin`、`codex-sandboxing` 等)あり、config システム・プロトコル型・rollout DB・plugin機構と密結合している。ファイル単位でのコード移植は、polaris にまだ存在しないそれらの周辺機構を丸ごと持ち込むことになり、「常時コンテキストを990トークン以下に保つ」という polaris の設計方針にも反する。そのため **コードの直接移植は行わず、技術選定と画面設計だけを参考にして polaris 自身の `Session` / `agent::run` に対する薄い TUI 層を新規に書く**。

## スコープ

含む:
- `polaris`(`--prompt` を省略した起動)でTUIに入る対話ループ
- スクロール可能な会話履歴ペイン + 下部入力欄の2ペイン構成
- ツール実行承認をTUI内でインタラクティブに行う(既存の `Gate`/`Approver` 境界はそのまま使う)
- セッションのディスク永続化と、次回起動時の再開

含まない(将来のM5後続、または別スコープ):
- 複数ペイン、diffビューアなど codex 相当の高度なUI
- セッションの圧縮(compaction)
- マルチプロバイダの切り替えUI
- マウス操作、シンタックスハイライト

## アーキテクチャ

### 案の比較

**案A(採用): 同期ブロッキング・ループ**
既存の `TerminalApprover`(`crates/polaris-cli/src/main.rs`)と同じ構え——ターン開始時に「thinking...」を一度描画してから `agent::run(...).await` の完了を待ち、`Approver::ask` が発火したら承認モーダルを描画して `crossterm::event::read()` でブロッキング入力を待つ。`polaris-core::approval::Approver` トレイトのシグネチャ(`fn ask(&mut self, reason: &str) -> Decision`)は変更不要。

**案B(不採用、将来の拡張先として記録): 非同期イベントループ**
エージェントをバックグラウンドタスクに投げ、`mpsc` チャンネルで進捗・承認要求を描画ループへ送り、`crossterm::EventStream` とチャンネルを同時に poll してスピナーをアニメーションさせる。応答性は高いが `Approver::ask` を非同期越しに待つ配線が要り、実装・レビューコストが増える。

案Aを採用する。理由は実装量が小さく既存のトレイト境界を変更せずに済むこと、そして関数分割(`render` / `handle_input` / `run_turn` を独立した関数にする)さえ守れば、将来のスピナー対応(案B)は差分追加で済むこと。

### クレート構成

新規クレート `crates/polaris-tui` を追加する(`polaris-cli` に直接書き込まない)。理由: `polaris-cli` は「引数解析とエントリポイント」という単一の責務を保っており、数百行規模になるTUIの描画・入力処理コードをそこに足すと役割が混ざる。`polaris-tui` は `polaris-core`(`Session`、`agent::run`、`Approver`/`Gate`)と `polaris-provider`、`ratatui`、`crossterm` に依存し、`polaris-cli` から呼び出される1つの公開エントリポイント関数(例: `polaris_tui::run(...)`)だけを公開する。

新規ワークスペース依存:
- `ratatui`(バックエンドは `crossterm`)
- `crossterm`

### 起動経路

`polaris-cli/src/main.rs` の `Args` で `prompt: Option<String>` かつ `command: None` のとき、既存の一発実行分岐の代わりに `polaris_tui::run(...)` を呼ぶ。`polaris login` / `polaris logout` / `--confined-apply` の既存分岐は変更しない。`--sandbox` / `--approval` / `--audit` / `--max-turns` はTUI起動時にも同じ意味で使う(一発実行と同じ `Args` をそのまま渡す)。

### 画面構成とイベントループ

```
+--------------------------------------+
| 会話履歴(スクロール可能)              |
| ...                                   |
+--------------------------------------+
| 入力欄                                 |
+--------------------------------------+
```

メインループ(擬似コード):
```
loop {
    render(history, input_buffer)
    match crossterm::event::read()? {
        Key(Enter) => {
            let text = take(&mut input_buffer)
            session.push_user(&text)
            persist_message(&text)  // ユーザー発話を先に永続化
            render(history, "thinking...")
            let reply = agent::run(..., &mut session, ..., &mut ctx).await
            persist_message(&reply)
            history.push(reply)
        }
        Key(other) => edit input_buffer
        Key(Ctrl-C) => break
        Resize => continue  // 次のループでrenderが追従
    }
}
```

`Approver::ask` の実装(`TuiApprover`)は、呼ばれた時点で承認モーダルを描画し、`crossterm::event::read()` を y/n が来るまでブロッキングで読む。これは一発実行の `TerminalApprover` の描画をTUI向けに差し替えただけで、`Gate::check` からの呼び出し方は変わらない。

### 永続化と再開

`crates/polaris-cli/src/main.rs` に既にある `default_state_dir(cwd)`(`project::resolve_root` からハッシュ化した project-id で `~/.polaris/state/<project-id>/` を返す)を再利用し、同じディレクトリに `tui-session.jsonl` を置く。1メッセージ1行の JSON(`polaris_provider::Message` は既に `Serialize`/`Deserialize` を derive 済み)を追記する。

起動時、`tui-session.jsonl` が存在すれば全行を読み込んで `Session` を復元してから会話履歴ペインに表示し、そのまま追記を続ける。存在しなければ空の `Session` から始める。破損行(パース失敗)が見つかった場合は、その行以降を切り捨てて警告を1行表示し、それより前の履歴で再開する(監査ログの伏字化と同様、無言で全損させない)。

明示的なセッションのリセットは、`tui-session.jsonl` を削除する操作(将来 `polaris tui --new` 等のフラグを足す余地はあるが、今回のスコープには含めない — ユーザーがファイルを消せば新規セッションになる、で足りる)。

### エラーハンドリング

- provider エラー(`AgentError::Provider`)はターンの失敗として履歴ペインにエラー行を1つ追加し、ループは継続する(プロセスは落とさない)。一発実行時は即終了するのと対照的——対話セッションを1回のAPIエラーで終わらせない。
- `AgentError::Stopped`(`max-turns` 超過など)は履歴ペインに理由を表示し、以後はそのターンの入力を受け付けない旨を示すが、TUI自体は終了しない(スクロールして過去ログは見られる)。
- 端末が対話端末でない(`stdin`/`stdout` が非TTY)場合は、TUIに入らずエラーで終了する。一発実行(`--prompt` 明示時)はTTYを要求しない現状の挙動を維持する。

### テスト方針

- `polaris-tui` 内: 描画関数とイベントハンドラをテスト用の `Backend`(`ratatui::backend::TestBackend`)で検証する単体テスト。実端末を必要としない。
- 永続化: JSONLの書き込み→読み込みで `Session` が一致することを確認するテスト、破損行の切り捨てを確認するテスト(`tempfile` を使い、既存の `polaris-core` のテストパターンに合わせる)。
- `TuiApprover` はキー入力をモックできるように `Approver` トレイト経由でテストする(実端末を使わない、既存の `Gate` のテストパターンを踏襲)。
- 実端末でのE2Eは自動化しない(既存の `polaris login` の「手で確かめる」節と同様、README に手動確認手順を追記する)。

## 未解決/次工程で決めること

- `polaris-tui` の公開関数のシグネチャ(`Args` をそのまま渡すか、専用の設定型を作るか)は実装計画(writing-plans)側で詰める。
- スクロール量・キーバインド(矢印キー、PageUp/PageDown程度を想定)の詳細は実装時に決める。
