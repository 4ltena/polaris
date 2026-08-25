# polaris TUI マウスドラッグ選択・自前クリップボードコピー 設計書

## 背景・目的

v0.6.0 "Spica" の開発中、会話履歴のホイールスクロール対応のため端末のマウスキャプチャ(`EnableMouseCapture`)を常時有効化したところ、端末側のネイティブなドラッグ選択・コピーが一切効かなくなる副作用が判明した。実際の codex(`codex-rs`、ローカルの `docs/superpowers/specs` と同じリポジトリ群内、`/Users/kn/File/projects/codex/upstream/codex-parallel/codex-rs` にチェックアウト済み)のソースを確認したところ、マウス処理コード自体が一切存在せず、マウスキャプチャを有効化しないことで端末ネイティブの選択を常に優先する設計だと分かった。この方針にいったん合わせ、polaris もマウスキャプチャを完全に廃止し、代わりに直近の返信をコピーする `/copy`・`Ctrl+O`(OSC 52 経由)を追加した。

しかしユーザーからは「ホイールスクロールと選択・コピーの両方を使いたい」という要望があり、xterm マウスプロトコルの仕様上この2つは同一の端末機能(マウスレポーティング)を奪い合う関係にあるため、端末レベルでの両立はできないことを確認済み(codex 自身もこのトレードオフを「マウス機能自体を持たない」ことで回避している)。

本設計では、マウスキャプチャを再度常時有効化した上で、polaris 自身がマウスの押下・ドラッグ・解放イベントを受け取ってテキスト選択を実装し、選択範囲を反転表示(reverse video)でハイライトし、マウス解放時に既存の `clipboard::copy_to_clipboard`(OSC 52)へ渡してコピーする。これにより「ホイールスクロール」と「選択してコピー」の両方を、端末ネイティブ選択には頼らずに polaris アプリ内で両立させる。

## スコープ

対象: `polaris-tui` の会話履歴表示エリアでのマウスドラッグ選択・ハイライト描画・コピー、およびホイールスクロールの復元。

非対象:
- 矩形(列単位)選択は対象外。ストリーム選択(通常のテキストエディタと同じ、開始行〜終了行を跨ぐ範囲選択)のみ実装する。
- フッター(入力欄)・各種ピッカー(`/model` 等)でのマウス選択は対象外。選択は会話履歴エリアに限定する。
- `/copy`・`Ctrl+O`(直近の返信をコピー)は変更しない。本機能は独立した別のコピー経路として共存する。
- タイマーによる「マウス静止時の継続的な自動スクロール」は対象外。ドラッグイベントが届くたびにフチ判定してスクロールする、イベント駆動の近似実装のみ行う(下記参照)。

## アーキテクチャ

```
現在: マウスキャプチャ完全廃止 → 端末ネイティブのドラッグ選択が常に効く。
      ホイールスクロールは無い(PageUp/PageDownキーのみ)。

変更後: マウスキャプチャを再度常時ON。
        polaris自身が Down/Drag/Up イベントを受け取り、
        selection.rs が「wrap_history_lines が毎フレーム計算する
        wrapped配列上の座標」として選択状態を保持する。
        描画は render_history_into の後に、選択範囲だけ
        Buffer のセルへ直接 Modifier::REVERSED を重ね掛けする
        追加パスとして行う(HistoryLine/wrap_history_lines/
        render_history_into 自体は無変更)。
        マウス解放時に選択テキストを抽出し、
        clipboard::copy_to_clipboard(OSC 52) へ渡す。
        ScrollUp/ScrollDownイベントは以前と同じくscroll_offsetを
        増減する(復元)。
```

### 1. 新規モジュール `crates/polaris-tui/src/selection.rs`

```rust
/// wrapped(折り返し後)配列上の位置。文字単位(バイトではなく)、
/// Unicode幅を考慮する既存のwrap_history_lines/input.rsの流儀に合わせる。
/// ratatui::layout::Position(画面上のカーソル座標)と紛らわしいため
/// あえて別名にする。
pub struct TextPos {
    pub line: usize, // wrapped配列のインデックス
    pub col: usize,  // その行内の文字インデックス
}

/// ドラッグ中(dragging=true)は毎Dragイベントでcursorを更新し、
/// mouse-up でdragging=falseになりコピーが走る。
pub struct Selection {
    pub anchor: TextPos,
    pub cursor: TextPos,
    pub dragging: bool,
}

impl Selection {
    /// anchor/cursorの前後関係を正規化し(下から上へドラッグしても
    /// 常に「開始が終了より前」になるようにする)、(start, end)を返す。
    pub fn ordered(&self) -> (TextPos, TextPos);
}

/// 画面座標(履歴エリア内の行・列、0始まりのバッファ相対)を
/// wrapped配列上のTextPosへ変換する。エリア外・windowの範囲外なら
/// 直近の有効な位置へクランプする(ドラッグ中に履歴エリアの外まで
/// マウスが出た場合でも選択が破綻しないようにするため)。
pub fn text_pos_from_screen(
    wrapped_len: usize,
    window: std::ops::Range<usize>,
    area: ratatui::layout::Rect,
    screen_row: u16,
    screen_col: u16,
) -> TextPos;

/// 選択範囲に含まれる各wrapped行について、ハイライトすべき
/// 文字列範囲(開始列, 終了列)を返す。中間行は「その行の実際の
/// 文字数」までであり、エリア幅いっぱいの空白パディングは含まない
/// (extract_textが返す文字列と、ハイライトされる範囲を一致させる
/// ため)。
pub fn highlighted_columns(
    wrapped: &[render::HistoryLine],
    sel: &Selection,
) -> Vec<(usize /* line */, usize /* start col */, usize /* end col */)>;

/// 選択範囲のテキストを抽出する。複数行にまたがる場合は"\n"で結合。
pub fn extract_text(wrapped: &[render::HistoryLine], sel: &Selection) -> String;
```

`text_pos_from_screen` の座標変換は、既存の `draw_frame` が `history_area` と `window = visible_history_window(...)` を毎フレーム計算しているのと同じ入力を使う:

- `wrapped_line_index = window.start + (screen_row - history_area.y)`(`history_area.height` を超えたらクランプ)
- 列方向は、対象行の `Line` を構成する `Span` を順に見て、`unicode_width::UnicodeWidthChar` で表示幅を積算しながら `screen_col - history_area.x` に達する文字インデックスを求める(`wrap_history_lines` の `push_row_trimmed` と同じ考え方)。

### 2. 選択状態の保持・クリア(`crates/polaris-tui/src/lib.rs`)

- `run()` に `let mut selection: Option<selection::Selection> = None;` を新設。
- `MouseEventKind::Down(MouseButton::Left)`: `history_area` 内であれば新規 `Selection { anchor: pos, cursor: pos, dragging: true }` を設定(既存の選択があれば上書き)。
- `MouseEventKind::Drag(MouseButton::Left)`: `selection.dragging == true` の間、`cursor` を更新。同時に「フチでの自動スクロール」(下記4節)を行う。
- `MouseEventKind::Up(MouseButton::Left)`: `selection.dragging = false` にし、`selection::extract_text` で得たテキストを `clipboard::copy_to_clipboard` へ渡し、結果を `Status::Notice` に反映する(空選択・1文字も無い場合は何もしない)。
- 次のいずれかで `selection = None` にする: 新規Down(上と同じ処理で上書きされるので実質no-op)、`apply_key` に渡る前のキー入力全般(`InputAction::Continue`/`Submit` いずれの手前でも)、`/new`・`/resume`・`/fork`・`/clear` などの会話リセット、ターミナルリサイズ検知(`Event::Resize`)。

### 3. 描画(`crates/polaris-tui/src/lib.rs` の `draw_frame`、`render.rs` に新規関数)

`render_history_into(buf, history_area, &wrapped[window])` の直後に:

```rust
pub fn apply_selection_highlight(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    window: std::ops::Range<usize>,
    wrapped: &[HistoryLine],
    sel: &Selection,
);
```

を呼ぶ。`selection::highlighted_columns` が返す各 `(line, start_col, end_col)` について、`line` が `window` に含まれる場合のみ(画面外の選択部分は無視)、対応する画面行 `area.y + (line - window.start)` の `start_col..end_col` のセルに `buf[(x, y)].modifier |= Modifier::REVERSED` を適用する(既存のスタイル・色はそのまま、反転だけ重ねる)。

### 4. フチでの自動スクロール

`Drag` イベント処理の中で、`screen_row <= history_area.y`(上端)なら `scroll_offset = scroll_offset.saturating_add(1)`、`screen_row >= history_area.y + history_area.height - 1`(下端)なら `scroll_offset = scroll_offset.saturating_sub(1)` を、ドラッグの `cursor` 更新と同じタイミングで行う。マウスを完全に静止させ続けた場合は追加の `Drag` イベントが来ないため止まるが、実用上はドラッグ操作自体に伴う微小な移動で継続的にスクロールされる想定。

### 5. マウスキャプチャ・ホイールスクロールの復元

- `run()` 冒頭で `ratatui::crossterm::event::EnableMouseCapture` を再度発行(v0.6.0 で一度削除したコードを復元)。終了時に `DisableMouseCapture` も同様に復元。
- アイドルループ(現状 `ratatui::crossterm::event::read()` による同期的ブロッキング)・ターン実行中の `tokio::select!` ループ双方で `Event::Mouse` を再度処理する。`MouseEventKind::ScrollUp`/`ScrollDown` は、v0.6.0 で一度削除した `MOUSE_SCROLL_LINES` 定数(3行)を復元してスクロール、`Down`/`Drag`/`Up` は本設計の選択処理を呼ぶ。
- `MID_TURN_NOTICE_DURATION` と同様のタイマー管理は不要(選択自体はイベント駆動のため、ターン中の 100ms ティックによる `status` 上書きと衝突しない — 選択のコピー結果 Notice は `Ctrl+O` と同じ「ターン中は短時間で消える」制約を受けるが、これは既存の `mid_turn_notice_until` 機構をそのまま再利用すればよい)。

## テスト方針

1. `selection.rs` の純粋関数群を単体テストする(実端末・実I/O不要):
   - `text_pos_from_screen`: エリア内の座標が正しい `TextPos` に変換されること、エリア外・window外の座標がクランプされること、全角文字を含む行での列変換が正しいこと。
   - `Selection::ordered`: 上→下ドラッグ・下→上ドラッグ双方で正しく正規化されること。
   - `highlighted_columns`: 単一行選択・複数行選択(中間行が行全体になること)。
   - `extract_text`: 単一行・複数行(`"\n"` 結合)・全角文字を含む選択の抽出結果。
2. `apply_selection_highlight` は `ratatui::buffer::Buffer` を直接組み立てて呼び出し、対象セルの `Modifier::REVERSED` が立っていること/対象外セルには立っていないことをテストする(既存の `render_history_into` のテストと同じ手法)。
3. mouse-up 時のコピー呼び出しは、`clipboard::copy_last_reply_with`/`copy_text_with` と同様に注入可能な `copy_fn` を取る形にして(例: `fn apply_selection_copy(text: &str, copy_fn: impl FnOnce(&str) -> Result<(), String>) -> String`)、実クリップボードI/Oなしにテストする。
4. マウスイベントを受け取ってから `selection` state を更新する `run()` 側のロジック(Down/Drag/Up の分岐、クリア条件)は、既存の `PageUp`/`Ctrl+O` 同様、実端末での結線までは自動テストの対象外とする(このプロジェクトの既存の慣習 — tmux での実地検証で担保する)。
5. 実装完了後、tmux でのライブ検証: ドラッグで選択・ハイライト表示・mouse-up での自動コピー・フチでの自動スクロール・ホイールスクロールの復元・キー入力での選択クリアを確認する。

## 既知の制約・将来課題

- リサイズ中に選択がクリアされる(ラップし直されると `wrapped` 上の座標が無効になりうるため、安全側に倒して破棄する)。
- フチでの自動スクロールはイベント駆動の近似であり、マウスを完全に静止させ続けた場合は止まる。真のタイマー駆動にするにはアイドルループを現状の同期的 `event::read()` からターン実行中と同様の `tokio::select!` ベースへ作り替える必要があり、本設計のスコープ外とする。
- 矩形選択は対象外。
