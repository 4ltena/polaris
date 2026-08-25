# polaris TUI 自前スクロール管理・フッター固定化 設計書

## 背景・目的

現在の polaris TUI は `ratatui::Viewport::Inline` を使い、ヘッダー・会話履歴・ライブツール進捗を `terminal.insert_before` でターミナルのネイティブスクロールバックへ一度きり印字し、フッター(model/effort/入力欄)だけを小さな固定領域として再描画している。この設計は codex 自身の実端末挙動に合わせたものだったが、ターミナルエミュレータ側の「新規出力・キー入力があると強制的に最下部へスクロールする」挙動(macOS 標準 Terminal.app はこれが既定でオフにできない)と衝突し、ユーザーが履歴を上にスクロールして読んでいる最中に何か入力すると強制的に最下部へ戻されてしまう問題がある。

cmux(Ghostty ベース)の `scroll-to-bottom = no-keystroke, no-output` 設定でターミナル側からこれを無効化できることを確認したが、Terminal.app にはこの設定自体が存在せず、ANSI 制御シーケンスレベルでも回避不能であることを tmux での実地検証で確認済み。したがって Terminal.app を含む全ターミナルで一貫した挙動を保証するには、ネイティブスクロールバックへの依存自体をやめ、polaris 自身が画面全体・スクロール位置・フッターの固定表示を自前管理する方式へ移行する。

## スコープ

対象: `polaris-tui` の描画方式全体(`Viewport::Inline` → `Viewport::Fullscreen`)。

非対象: `polaris-cli`(一発実行・headless)は元々スクロール概念が無く無関係。フッター自体の見た目・ロジック(`render_footer`)は変更しない(そのまま流用)。IME カーソル位置計算(直近の修正)もそのまま活きる。

## アーキテクチャ

```
現在:  [ヘッダー/履歴] --insert_before--> ターミナルのネイティブscrollback(アプリの管理外)
       [フッター]      --毎フレームdraw--> Viewport::Inline の小領域のみ

変更後: [ヘッダー/履歴/フッター] 全て同じ Viewport::Fullscreen 内で、
        毎フレーム polaris 自身が計算した領域に再描画する。
        履歴は永続 Vec<HistoryLine> に蓄積し続け、scroll_offset に応じた
        「窓」だけを毎フレーム切り出して描画する。ネイティブscrollbackは
        一切使わない。
```

### 1. 履歴の永続化(`crates/polaris-tui/src/lib.rs`)

現状の `print_new_history`/`print_live_event` は `terminal.insert_before` で一度だけ印字し、二度と触らない。これを次のように変更する:

- `run()` 内に `history: Vec<render::HistoryLine>` を新設し、セッションを通じて保持する。
- `print_new_history` → `append_new_history` に改名し、`insert_before` する代わりに `history.extend(lines)` する。
- `print_live_event` → `append_live_event` に改名し、同様に `history.extend(...)` する。
- `/resume`・`/new`・`/fork`・`/clear` など履歴を差し替える箇所は、既存の `printed_messages`/`printed_local_lines` カウンタのリセットに加えて `history.clear()` も行う(これらの呼び出し箇所は既存コードで特定済み — 全て `printed_messages = 0` 等と同じタイミングで揃える)。

**見落としていた既存コンポーネント、ここで確定させる**: 起動時に一度だけ `insert_before(render::HEADER_HEIGHT, ...)` で印字しているヘッダーボックス(`render_header_into`、罫線付きの `✦ polaris` 枠)も同じ理由で `history` へ移す必要がある。`render_header_into` はバッファへ直接描画する関数で `HistoryLine` を返さないため、新たに `render::header_history_lines(header: &HeaderInfo) -> Vec<HistoryLine>` を追加し、罫線(上端行・各内容行の左右`│`・下端行)を `header_lines(header)`(既存の非罫線コンテンツ行を返す関数)から手組みで `HistoryLine` として構築する。起動時はこの関数の戻り値を `history` へ `extend` する(`insert_before` は呼ばない)。`render_header_into`/`HEADER_HEIGHT` の直接呼び出しは廃止する。

### 2. スクロール状態

- `scroll_offset: usize` を新設(最下部から何行上にスクロールしているか)。`0` は「常に最新行を追従表示」を意味し、新しい行が `history` に追加されても `scroll_offset` 自体は変更しない — 「窓」の計算が `history.len()` を毎回参照するため、`scroll_offset == 0` のまま自動的に最新行へ追従する(tail -f 相当。追加のフラグは不要)。
- 表示ウィンドウの計算(新設するヘルパー関数、例 `visible_history_window(history_len: usize, scroll_offset: usize, visible_height: usize) -> Range<usize>`):
  - `max_scroll = history_len.saturating_sub(visible_height)`(それ以上スクロールできない上限)
  - `effective_scroll = scroll_offset.min(max_scroll)`
  - `end = history_len - effective_scroll`
  - `start = end.saturating_sub(visible_height)`
  - 戻り値 `start..end`
  - この関数は純粋関数として `render.rs` に置き、単体テストしやすくする。

### 3. キー操作

- メインループの入力ハンドリング(`crates/polaris-tui/src/lib.rs` の `KeyCode` 分岐)に、既存の「候補ポップアップ表示中の Up/Down」処理の**後**(=候補が空の場合のみ到達する分岐)で `KeyCode::Up`/`KeyCode::Down` を新たに処理する:
  - `Up`: `scroll_offset = scroll_offset.saturating_add(1)`(表示時に `max_scroll` でクランプされるので範囲外チェックは不要)
  - `Down`: `scroll_offset = scroll_offset.saturating_sub(1)`
  - どちらも `continue`(再描画のみ、`apply_key` は呼ばない)
- 現状 `apply_key`(`input.rs`)は Up/Down を一切処理していないため、既存動作との衝突は無い。
- メッセージ送信時(`session.push_user` の直前/直後)に `scroll_offset = 0` にリセットする(承認済み: 送信したら自動で最下部へ戻る)。

### 4. 描画ループ

`run()` の毎フレーム描画箇所(トップオブループの `terminal.borrow_mut().draw(...)` と、ターン中の 100ms ティック内の描画)を統一し、次の内容を単一の `draw` クロージャで行う:

1. `frame.area()` を `Layout::vertical` で「履歴領域」と「フッター領域(既存の `INLINE_VIEWPORT_HEIGHT` 相当の固定高さ)」に分割する。
2. `visible_history_window(...)` で現在の窓を求め、`render::render_history_into(buf, history_area, &history[window])` で履歴領域へ描画する(`render_history_into` は既存関数をそのまま流用可能 — 窓を渡すだけで変更不要)。
3. `render::render_footer(...)` をフッター領域へ描画する(変更なし)。

ターン中の 100ms ティック(シマー演出)による再描画は、`history`/`scroll_offset` を変更しないので、単に同じ窓を再描画するだけになり、ユーザーが履歴を読んでいる位置を一切乱さない。

### 5. ターミナル初期化・後始末

- `run()` 冒頭の `ratatui::init_with_options(TerminalOptions { viewport: Viewport::Inline(...) })` を `Viewport::Fullscreen` に変更する。
- 既存の `ratatui::restore()` 呼び出し(2箇所)はそのまま — `Viewport::Fullscreen` の場合も alternate screen ・ raw mode を正しく後始末する(`init`/`restore` は対になっている)。

### 6. フルスクリーンピッカーの簡略化

`with_fullscreen_picker`(`/model`・`/skills`・`/permissions`・`/resume` 用に一時的に別の `Terminal` インスタンスへ切り替えるトリック、および直近追加した「切り替え前に `clear()` する」修正)は、アプリ全体が常時 `Viewport::Fullscreen` になることで**丸ごと不要になる**。各ピッカー(`handle_resume`・`handle_permissions`・`handle_model`・`run_skills_picker`)は、既存の同じ `terminal: &RefCell<Terminal<...>>` に対して直接 `terminal.borrow_mut().draw(|f| render::render_xxx_picker(f, ...))` を呼ぶだけでよくなる。`with_fullscreen_picker` 関数自体と、その呼び出し箇所(4箇所)を削除し、各ハンドラの `Terminal` 引数はそのまま(型は変わらず、渡し方が単純化されるだけ)。

## テスト方針

1. `visible_history_window` の純粋関数テスト: 履歴が窓より少ない場合・ちょうど・多い場合、`scroll_offset` が 0/中間/上限超過の場合それぞれで正しい `Range` を返すこと。
2. `render_history_into` は既存のまま(窓を渡すだけなので新規テスト不要、既存テストが引き続き通ることを確認)。
3. Up/Down キーで `scroll_offset` が増減し、候補ポップアップ表示中は従来通り候補選択に使われ続けること(既存テストのシナリオを壊さないことを確認)。
4. メッセージ送信で `scroll_offset` が 0 にリセットされること。
5. `/resume`・`/new`・`/fork`・`/clear` で `history` がクリアされ、新しい内容だけが表示されること。
6. `with_fullscreen_picker` 削除後もピッカー(`/model` 等)が正しく描画・選択・確定できること(既存のピッカーテストが引き続き通ることを確認)。
7. `header_history_lines` が `render_header_into`(既存)と視覚的に同じ罫線・内容を持つ行を返すこと(罫線文字・内容行の対応を直接アサートする)。

## 受け入れ基準

1. 履歴をスクロールしている最中に、ターン中のシマー演出(100ms ティック)やライブツール進捗イベントが追加されても、表示位置(スクロール位置)が動かない。
2. 履歴をスクロールしている最中に文字を入力しても、強制的に最下部へ戻されない。
3. 新しいメッセージを送信すると、自動的に最下部(最新行)へ戻る。
4. フッター(model/effort/入力欄)は常に画面最下部に固定表示される。
5. `polaris-cli`(一発実行・headless)の既存挙動に影響がない。
6. `/model`・`/skills`・`/permissions`・`/resume` の各ピッカーが `with_fullscreen_picker` 削除後も従来通り動作する。

## 見送った代替案

- **cmux/Ghostty 限定対応**: `scroll-to-bottom` 設定はターミナル側で解決できるが、OSS として特定ターミナルに依存させるべきではないと判断し見送った(README に Tips として案内するのみに留める)。
- **PageUp/PageDown・マウスホイール対応**: 今回は Up/Down キーのみに限定(承認済み)。将来的な拡張の余地はあるが、今回のスコープ外とする。
- **Home/End キーでの先頭/末尾ジャンプ**: 実装コストは低いが、今回のユーザー要望には含まれていないため見送った。
