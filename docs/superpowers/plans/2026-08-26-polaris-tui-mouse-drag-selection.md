# polaris TUI マウスドラッグ選択・自前クリップボードコピー Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-enable terminal mouse capture in `polaris-tui` (removed in commit `11adace`), restore wheel-scroll, and add app-level mouse drag-to-select over the conversation history — so wheel-scroll and select-and-copy coexist without depending on terminal-native selection (which mouse capture blocks entirely; confirmed no terminal-level way to have both).

**Architecture:** A new `crates/polaris-tui/src/selection.rs` module holds pure coordinate-mapping/highlight/extraction functions operating on the same `wrapped: Vec<HistoryLine>` array `draw_frame` already recomputes every frame (via `render::wrap_history_lines`) — no changes to `HistoryLine`/`wrap_history_lines`/`render_history_into` themselves. Selection state (`Option<selection::Selection>`) lives in `run()`'s local scope next to `scroll_offset`, driven by `MouseEventKind::Down/Drag/Up(MouseButton::Left)` in both the idle loop and the mid-turn `tokio::select!` loop. Highlighting is a post-render overlay pass on the `ratatui::buffer::Buffer` (`Modifier::REVERSED`, no color changes). Copying reuses the existing `clipboard::copy_to_clipboard` (OSC 52) via a new small injectable-`copy_fn` wrapper, mirroring the pattern `clipboard::copy_text_with`/`lib.rs`'s `apply_copy_action` already established for `/copy`/`Ctrl+O`.

**Tech Stack:** Rust, `ratatui` 0.29 (`Buffer`/`Cell::set_style`, confirmed via the vendored source at `~/.cargo/registry/src/index.crates.io-*/ratatui-0.29.0/src/buffer/cell.rs` that `set_style` only merges `add_modifier`/`sub_modifier` and leaves `fg`/`bg` untouched when the passed `Style` carries no color), `crossterm` 0.28.1 (`MouseEventKind::{Down,Up,Drag}(MouseButton)`, `MouseButton::Left`, `Event::Resize(u16, u16)` — confirmed via the vendored source at `~/.cargo/registry/src/index.crates.io-*/crossterm-0.28.1/src/event.rs`), `unicode-width` (already a dependency, used identically in `wrap_history_lines`).

**Spec:** `docs/superpowers/specs/2026-08-26-polaris-tui-mouse-drag-selection-design.md`

## Global Constraints

- Full verification before every task's commit, exactly: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`.
- Commits: Conventional-Commits-style prefix, trailer `Co-Authored-By: Claude <noreply@anthropic.com>`, using the repository's own configured git identity (GitHub `@4ltena`) — never a hardcoded or harness-injected email.
- **Another Claude Code session is concurrently and legitimately editing unrelated files in this same repository right now** (a prompt-cache-key feature, uncommitted, touching `crates/polaris-cli/src/main.rs`, `crates/polaris-core/src/agent.rs`, `crates/polaris-provider/src/{lib,codex,openai}.rs`). Every task in this plan only touches files under `crates/polaris-tui/` plus this plan/spec doc. **Never run `git add -A` or `git add .`** — always `git add` the exact files this task changed, by name. If `cargo clippy --workspace`/`cargo fmt --all -- --check` reports errors in files this plan doesn't touch, that's the other session's in-progress work — do not fix it, do not stage it, note it and move on; the Global Constraint above still applies to *this plan's own files*.
- Stream selection only (no rectangular/block selection) — see spec's non-scope list.
- Selection is scoped to the conversation history area only; the footer (input box, suggestion popup, `/model`-etc. pickers) is out of scope.
- `/copy` and `Ctrl+O` (copies the last reply) are unchanged and coexist independently with this feature.
- Edge auto-scroll during a drag is event-driven (re-evaluated on every `Drag` event), not timer-driven — no change to the idle loop's synchronous `event::read()` architecture. This was an explicit, approved trade-off in the spec, not a gap.
- Selection clears on: a new `Down` (naturally, by being overwritten), any key event that reaches `apply_key`, every `/new`/`/resume`/`/fork`/`/clear` conversation reset (all four already funnel through the single `reset_conversation_view` helper — see Task 9), and `Event::Resize`.
- `ratatui::layout::Position` is already imported in `render.rs` for screen-cursor placement — this plan's new position type is named `TextPos` specifically to avoid colliding with that name/concept (a *text*-space position, not a *screen* position).

---

### Task 1: `selection.rs` — `TextPos`, `Selection`, `Selection::ordered`

**Files:**
- Create: `crates/polaris-tui/src/selection.rs`
- Modify: `crates/polaris-tui/src/lib.rs:5` (add `mod selection;` — insert alphabetically, after `pub mod render;` and before `pub mod sessions;`, matching the existing `mod` list's ordering)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct TextPos {
      pub line: usize,
      pub col: usize,
  }

  #[derive(Debug, Clone)]
  pub struct Selection {
      pub anchor: TextPos,
      pub cursor: TextPos,
      pub dragging: bool,
  }

  impl Selection {
      pub fn ordered(&self) -> (TextPos, TextPos);
  }
  ```
  Later tasks (2, 3, 4, 5, 8, 11) construct/read `Selection`/`TextPos` and call `.ordered()`.

- [ ] **Step 1: Write the failing tests**

Create `crates/polaris-tui/src/selection.rs` with just this much so the test module compiles against real types:

```rust
//! Mouse drag-to-select over the conversation history. Operates entirely
//! in the coordinate space of `render::wrap_history_lines`'s output (the
//! `wrapped` array `draw_frame` recomputes every frame) — never touches
//! `HistoryLine`/`wrap_history_lines`/`render_history_into` themselves.
//! See `docs/superpowers/specs/2026-08-26-polaris-tui-mouse-drag-selection-design.md`.

/// A position in the `wrapped` array's coordinate space: `line` indexes
/// into `wrapped`, `col` is a *character* index (not byte, not display
/// column) into that line's rendered text. Deliberately not named
/// `Position` — `render.rs` already imports `ratatui::layout::Position`
/// for screen-cursor placement, a different concept entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPos {
    pub line: usize,
    pub col: usize,
}

/// `dragging` is `true` from the initial `Down` through every `Drag`,
/// and set to `false` on `Up` (at which point the selected text is
/// copied — see `lib.rs`'s wiring in Tasks 8/11).
#[derive(Debug, Clone)]
pub struct Selection {
    pub anchor: TextPos,
    pub cursor: TextPos,
    pub dragging: bool,
}

impl Selection {
    /// Normalizes `anchor`/`cursor` into `(start, end)` regardless of
    /// which direction the drag went (top-to-bottom, bottom-to-top, or
    /// right-to-left within one line).
    pub fn ordered(&self) -> (TextPos, TextPos) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: usize, col: usize) -> TextPos {
        TextPos { line, col }
    }

    #[test]
    fn ordered_keeps_a_top_to_bottom_drag_as_is() {
        let sel = Selection {
            anchor: pos(2, 3),
            cursor: pos(5, 1),
            dragging: false,
        };
        assert_eq!(sel.ordered(), (pos(2, 3), pos(5, 1)));
    }

    #[test]
    fn ordered_flips_a_bottom_to_top_drag() {
        let sel = Selection {
            anchor: pos(5, 1),
            cursor: pos(2, 3),
            dragging: false,
        };
        assert_eq!(sel.ordered(), (pos(2, 3), pos(5, 1)));
    }

    #[test]
    fn ordered_flips_a_right_to_left_drag_on_the_same_line() {
        let sel = Selection {
            anchor: pos(4, 10),
            cursor: pos(4, 2),
            dragging: false,
        };
        assert_eq!(sel.ordered(), (pos(4, 2), pos(4, 10)));
    }

    #[test]
    fn ordered_is_stable_for_a_zero_width_selection() {
        let sel = Selection {
            anchor: pos(3, 5),
            cursor: pos(3, 5),
            dragging: true,
        };
        assert_eq!(sel.ordered(), (pos(3, 5), pos(3, 5)));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p polaris-tui selection::tests -- --nocapture`
Expected: FAIL — `not yet implemented` panics from the `todo!()`.

- [ ] **Step 3: Implement `Selection::ordered`**

Replace the `todo!()` body:

```rust
pub fn ordered(&self) -> (TextPos, TextPos) {
    let a = (self.anchor.line, self.anchor.col);
    let b = (self.cursor.line, self.cursor.col);
    if a <= b {
        (self.anchor, self.cursor)
    } else {
        (self.cursor, self.anchor)
    }
}
```

(Tuple comparison on `(line, col)` gives exactly the lexicographic ordering needed: earlier line wins regardless of column; same line compares by column.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p polaris-tui selection::tests`
Expected: PASS, 4 tests.

- [ ] **Step 5: Add the module and verify the whole workspace**

Edit `crates/polaris-tui/src/lib.rs`:

```rust
pub mod approver;
mod clipboard;
pub mod input;
pub mod onboarding;
pub mod persist;
pub mod render;
mod selection;
pub mod sessions;
pub mod slash;
mod time;
```

(`selection` is private like `clipboard`/`time` — nothing outside `polaris-tui` needs it.)

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

If clippy/fmt report anything outside `crates/polaris-tui/`, that's the other concurrent session's in-progress work — leave it, don't fix it, don't stage it (see Global Constraints).

- [ ] **Step 6: Commit**

```bash
git add crates/polaris-tui/src/selection.rs crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): add TextPos/Selection types for mouse drag-selection

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: `selection.rs` — `text_pos_from_screen`

**Files:**
- Modify: `crates/polaris-tui/src/selection.rs`

**Interfaces:**
- Consumes: `TextPos` (Task 1).
- Produces:
  ```rust
  pub fn text_pos_from_screen(
      wrapped_len: usize,
      window: std::ops::Range<usize>,
      area: ratatui::layout::Rect,
      screen_row: u16,
      screen_col: u16,
  ) -> TextPos
  ```
  Later tasks (8, 11) call this to convert a `MouseEvent`'s `row`/`column` into a `TextPos` on `Down`/`Drag`.

- [ ] **Step 1: Write the failing tests**

Add to `selection.rs`'s test module:

```rust
use ratatui::layout::Rect;

fn area(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect { x, y, width, height }
}

#[test]
fn a_point_inside_the_area_maps_to_the_matching_wrapped_line() {
    // area starts at (0, 0), 10 rows tall; window shows wrapped[0..10].
    // row 3 -> wrapped[3]; col is left as-is (col-mapping tested separately below).
    let pos = text_pos_from_screen(10, 0..10, area(0, 0, 40, 10), 3, 0);
    assert_eq!(pos.line, 3);
}

#[test]
fn the_window_start_offsets_which_wrapped_line_a_row_maps_to() {
    // window is 20..30 (scrolled back) -> row 0 is wrapped[20], row 3 is wrapped[23].
    let pos = text_pos_from_screen(30, 20..30, area(0, 0, 40, 10), 3, 0);
    assert_eq!(pos.line, 23);
}

#[test]
fn an_area_offset_from_the_screen_origin_is_subtracted_first() {
    // area starts at y=5 (e.g. below a header) -> screen_row=7 is the area's row 2.
    let pos = text_pos_from_screen(10, 0..10, area(0, 5, 40, 10), 7, 0);
    assert_eq!(pos.line, 2);
}

#[test]
fn a_row_past_the_area_or_window_clamps_to_the_last_visible_line() {
    let pos = text_pos_from_screen(10, 0..10, area(0, 0, 40, 5), 50, 0);
    // area is only 5 rows tall (rows 0..5 -> wrapped[0..5]); clamp to the last, wrapped[4].
    assert_eq!(pos.line, 4);
}

#[test]
fn a_row_above_the_area_clamps_to_the_first_visible_line() {
    let pos = text_pos_from_screen(10, 3..8, area(0, 10, 40, 5), 2, 0);
    // screen_row (2) is above area.y (10) -> clamp to the window's first line.
    assert_eq!(pos.line, 3);
}

#[test]
fn an_empty_wrapped_array_clamps_to_line_zero() {
    let pos = text_pos_from_screen(0, 0..0, area(0, 0, 40, 10), 3, 0);
    assert_eq!(pos.line, 0);
}

#[test]
fn column_zero_maps_to_char_index_zero() {
    let pos = text_pos_from_screen(10, 0..10, area(2, 0, 40, 10), 0, 2);
    // area.x = 2, screen_col = 2 -> col-within-area = 0.
    assert_eq!(pos.col, 0);
}

#[test]
fn a_column_before_the_area_clamps_to_zero() {
    let pos = text_pos_from_screen(10, 0..10, area(5, 0, 40, 10), 0, 1);
    assert_eq!(pos.col, 0);
}
```

Note: these tests only pin `line` and a simple `col` (area-relative offset, not yet Unicode-width-aware against real line content) — Task 3 (`highlighted_columns`) and Task 4 (`extract_text`) are what actually need to walk a `Line`'s `Span`s for real text. Keep `text_pos_from_screen`'s own `col` as `(screen_col - area.x)` clamped to `>= 0` — later tasks that need the exact character boundary within real content re-derive it themselves from `wrapped[pos.line]` rather than trusting a raw column offset as a character index (a raw offset can land mid-wide-character; see Task 3's own width-aware walk). Document this clearly in the doc comment (Step 3 below).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p polaris-tui selection::tests -- --nocapture`
Expected: FAIL with "cannot find function `text_pos_from_screen`".

- [ ] **Step 3: Implement it**

```rust
/// Converts a screen coordinate (as carried by `crossterm`'s
/// `MouseEvent::{row,column}`) into a `TextPos` in the `wrapped` array's
/// coordinate space. Clamps to the nearest valid line/column rather than
/// returning `Option` — a drag that strays outside `area` (common: the
/// user's mouse leaves the history region while still holding the
/// button) should extend the selection to the nearest edge, not silently
/// stop updating.
///
/// `col` here is simply `screen_col - area.x`, clamped — an area-relative
/// offset, not yet resolved against a specific line's actual character
/// boundaries (a raw offset can land in the middle of a wide/full-width
/// character). Callers that need the true character index for a specific
/// `wrapped` line re-derive it from that line's `Span`s directly (see
/// `highlighted_columns`/`extract_text` in Tasks 3/4) rather than trusting
/// this value as an exact character index.
pub fn text_pos_from_screen(
    wrapped_len: usize,
    window: std::ops::Range<usize>,
    area: ratatui::layout::Rect,
    screen_row: u16,
    screen_col: u16,
) -> TextPos {
    if wrapped_len == 0 {
        return TextPos { line: 0, col: 0 };
    }

    let row_in_area = screen_row.saturating_sub(area.y);
    let max_row_in_area = area.height.saturating_sub(1);
    let clamped_row = row_in_area.min(max_row_in_area);
    let line = (window.start + clamped_row as usize).min(wrapped_len.saturating_sub(1));

    let col = screen_col.saturating_sub(area.x) as usize;

    TextPos { line, col }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p polaris-tui selection::tests`
Expected: PASS, all tests from Task 1 + this task's 8 new ones.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/selection.rs
git commit -m "feat(polaris-tui): map screen coordinates to selection.rs's TextPos

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: `selection.rs` — `highlighted_columns`

**Files:**
- Modify: `crates/polaris-tui/src/selection.rs`

**Interfaces:**
- Consumes: `Selection`, `TextPos`, `Selection::ordered` (Task 1); `render::HistoryLine` (existing, `render.rs:405`).
- Produces:
  ```rust
  pub fn highlighted_columns(
      wrapped: &[crate::render::HistoryLine],
      sel: &Selection,
  ) -> Vec<(usize, usize, usize)> // (line, start_col, end_col), end_col exclusive
  ```
  Task 5 (`apply_selection_highlight`) calls this to know which cells to reverse-video.

Each returned tuple's `start_col`/`end_col` are **character indices** (not display columns, not bytes) into that `wrapped` line's full text (all its spans concatenated) — Task 5 walks the line's spans the same width-aware way `wrap_history_lines` already does to turn a character index into a screen column when it applies the highlight.

- [ ] **Step 1: Write the failing tests**

Add to `selection.rs`. First, a tiny local helper the tests need (character count of a `HistoryLine`, walking its spans — the same shape Task 4 will also need, but keep each task's own test helpers local/private and not shared yet; a real shared helper only gets extracted if Task 4 turns out to need the exact same one, which it does — see Task 4 Step 1's note):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::HistoryLine;
    use ratatui::text::Line;

    fn hl(text: &str) -> HistoryLine {
        HistoryLine {
            line: Line::from(text.to_string()),
            shaded: false,
        }
    }

    fn sel(anchor: (usize, usize), cursor: (usize, usize)) -> Selection {
        Selection {
            anchor: TextPos { line: anchor.0, col: anchor.1 },
            cursor: TextPos { line: cursor.0, col: cursor.1 },
            dragging: false,
        }
    }

    #[test]
    fn a_single_line_selection_highlights_only_its_own_column_range() {
        let wrapped = vec![hl("hello world")];
        let got = highlighted_columns(&wrapped, &sel((0, 2), (0, 7)));
        assert_eq!(got, vec![(0, 2, 7)]);
    }

    #[test]
    fn a_multi_line_selection_highlights_the_first_line_from_its_start_to_its_end() {
        let wrapped = vec![hl("first line"), hl("second line"), hl("third")];
        let got = highlighted_columns(&wrapped, &sel((0, 6), (2, 3)));
        // first line (index 0): from col 6 to its own length (10)
        assert_eq!(got[0], (0, 6, 10));
    }

    #[test]
    fn a_multi_line_selection_highlights_middle_lines_fully_at_their_own_length() {
        let wrapped = vec![hl("first line"), hl("second line"), hl("third")];
        let got = highlighted_columns(&wrapped, &sel((0, 6), (2, 3)));
        // second line (index 1) is fully included: 0..its own char count (11)
        assert_eq!(got[1], (1, 0, 11));
    }

    #[test]
    fn a_multi_line_selection_highlights_the_last_line_from_its_start_to_the_cursor() {
        let wrapped = vec![hl("first line"), hl("second line"), hl("third")];
        let got = highlighted_columns(&wrapped, &sel((0, 6), (2, 3)));
        // third line (index 2): from 0 to col 3
        assert_eq!(got[2], (2, 0, 3));
    }

    #[test]
    fn a_zero_width_selection_highlights_nothing() {
        let wrapped = vec![hl("hello")];
        let got = highlighted_columns(&wrapped, &sel((0, 2), (0, 2)));
        assert!(got.is_empty());
    }

    #[test]
    fn a_backward_drag_is_normalized_before_computing_columns() {
        let wrapped = vec![hl("hello world")];
        let got = highlighted_columns(&wrapped, &sel((0, 7), (0, 2)));
        assert_eq!(got, vec![(0, 2, 7)]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p polaris-tui selection::tests -- --nocapture`
Expected: FAIL with "cannot find function `highlighted_columns`".

- [ ] **Step 3: Implement it**

```rust
/// Character count of a `HistoryLine`'s full rendered text (all spans
/// concatenated) — used to clamp/compute "to the end of this line"
/// without needing to know display width here (`Modifier::REVERSED` is
/// applied per-cell downstream in `apply_selection_highlight`, which is
/// where width actually matters).
fn char_count(hl: &crate::render::HistoryLine) -> usize {
    hl.line.spans.iter().map(|s| s.content.chars().count()).sum()
}

pub fn highlighted_columns(
    wrapped: &[crate::render::HistoryLine],
    sel: &Selection,
) -> Vec<(usize, usize, usize)> {
    let (start, end) = sel.ordered();
    if start == end {
        return Vec::new();
    }

    let mut out = Vec::new();
    for line_idx in start.line..=end.line.min(wrapped.len().saturating_sub(1)) {
        let Some(hl) = wrapped.get(line_idx) else {
            break;
        };
        let len = char_count(hl);
        let (from, to) = if start.line == end.line {
            (start.col.min(len), end.col.min(len))
        } else if line_idx == start.line {
            (start.col.min(len), len)
        } else if line_idx == end.line {
            (0, end.col.min(len))
        } else {
            (0, len)
        };
        if from < to {
            out.push((line_idx, from, to));
        }
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p polaris-tui selection::tests`
Expected: PASS, all tests from Tasks 1-3.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/selection.rs
git commit -m "feat(polaris-tui): compute per-line highlight ranges for a selection

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: `selection.rs` — `extract_text`

**Files:**
- Modify: `crates/polaris-tui/src/selection.rs`

**Interfaces:**
- Consumes: `Selection`, `highlighted_columns` (Task 3) — reused directly rather than re-deriving the same line/column ranges.
- Produces:
  ```rust
  pub fn extract_text(wrapped: &[crate::render::HistoryLine], sel: &Selection) -> String
  ```
  Task 8/11 call this on `Up` to get the text to copy.

- [ ] **Step 1: Write the failing tests**

`highlighted_columns` already gives `(line, start_char, end_char)` ranges; `extract_text` only needs to slice each line's concatenated text by character index and join with `"\n"`. Reuse `char_count`'s sibling — a `line_text(hl) -> String` helper (the full concatenated text, needed here since `highlighted_columns` only returns indices, not text):

```rust
#[cfg(test)]
mod tests {
    // ... (existing `hl`/`sel` helpers from Task 3 stay; add these tests)

    #[test]
    fn extract_text_returns_a_single_lines_substring() {
        let wrapped = vec![hl("hello world")];
        let got = extract_text(&wrapped, &sel((0, 0), (0, 5)));
        assert_eq!(got, "hello");
    }

    #[test]
    fn extract_text_joins_multiple_lines_with_newlines() {
        let wrapped = vec![hl("first line"), hl("second line"), hl("third")];
        let got = extract_text(&wrapped, &sel((0, 6), (2, 3)));
        assert_eq!(got, "line\nsecond line\nthi");
    }

    #[test]
    fn extract_text_is_empty_for_a_zero_width_selection() {
        let wrapped = vec![hl("hello")];
        let got = extract_text(&wrapped, &sel((0, 2), (0, 2)));
        assert_eq!(got, "");
    }

    #[test]
    fn extract_text_handles_full_width_characters_by_character_not_byte() {
        let wrapped = vec![hl("aあいbうc")]; // full-width chars mixed with ASCII
        // select "あい" — chars 1..3 (a=0, あ=1, い=2, b=3, ...)
        let got = extract_text(&wrapped, &sel((0, 1), (0, 3)));
        assert_eq!(got, "あい");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p polaris-tui selection::tests -- --nocapture`
Expected: FAIL with "cannot find function `extract_text`".

- [ ] **Step 3: Implement it**

```rust
/// The full rendered text of one `HistoryLine` (all its spans
/// concatenated) — `char_count`'s sibling, needed here (unlike in
/// `highlighted_columns`) because this function returns text, not just
/// counts.
fn line_text(hl: &crate::render::HistoryLine) -> String {
    hl.line.spans.iter().map(|s| s.content.as_ref()).collect()
}

pub fn extract_text(wrapped: &[crate::render::HistoryLine], sel: &Selection) -> String {
    highlighted_columns(wrapped, sel)
        .into_iter()
        .filter_map(|(line_idx, from, to)| {
            let text = line_text(wrapped.get(line_idx)?);
            Some(text.chars().skip(from).take(to - from).collect::<String>())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p polaris-tui selection::tests`
Expected: PASS, all tests from Tasks 1-4.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/selection.rs
git commit -m "feat(polaris-tui): extract selected text across multiple history lines

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: `render.rs` — `apply_selection_highlight`

**Files:**
- Modify: `crates/polaris-tui/src/render.rs` (new `pub fn`, near `render_history_into`)

**Interfaces:**
- Consumes: `selection::highlighted_columns` (Task 3), `selection::Selection` (Task 1).
- Produces:
  ```rust
  pub fn apply_selection_highlight(
      buf: &mut ratatui::buffer::Buffer,
      area: ratatui::layout::Rect,
      window: std::ops::Range<usize>,
      wrapped: &[HistoryLine],
      sel: &Selection,
  )
  ```
  Task 9 calls this from `draw_frame`, right after `render_history_into`.

- [ ] **Step 1: Write the failing tests**

Add to `render.rs`'s existing `#[cfg(test)] mod tests` block (near `render_history_into`'s own tests — grep for `fn render_history_into` in the test module to find them):

```rust
#[test]
fn apply_selection_highlight_reverses_only_the_selected_cells() {
    use crate::selection::{Selection, TextPos};
    let wrapped = vec![HistoryLine::plain(Line::from("hello world"))];
    let area = ratatui::layout::Rect::new(0, 0, 20, 5);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    render_history_into(&mut buf, area, &wrapped);

    let sel = Selection {
        anchor: TextPos { line: 0, col: 2 },
        cursor: TextPos { line: 0, col: 7 },
        dragging: false,
    };
    apply_selection_highlight(&mut buf, area, 0..1, &wrapped, &sel);

    for x in 0..2 {
        assert!(
            !buf[(x, 0)].modifier.contains(Modifier::REVERSED),
            "column {x} should not be highlighted"
        );
    }
    for x in 2..7 {
        assert!(
            buf[(x, 0)].modifier.contains(Modifier::REVERSED),
            "column {x} should be highlighted"
        );
    }
    for x in 7..20 {
        assert!(
            !buf[(x, 0)].modifier.contains(Modifier::REVERSED),
            "column {x} should not be highlighted"
        );
    }
}

#[test]
fn apply_selection_highlight_ignores_lines_outside_the_visible_window() {
    use crate::selection::{Selection, TextPos};
    let wrapped = vec![
        HistoryLine::plain(Line::from("scrolled off the top")),
        HistoryLine::plain(Line::from("visible line")),
    ];
    let area = ratatui::layout::Rect::new(0, 0, 30, 5);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    // window is 1..2 -- only wrapped[1] is on screen, at row 0.
    render_history_into(&mut buf, area, &wrapped[1..2]);

    let sel = Selection {
        anchor: TextPos { line: 0, col: 0 },
        cursor: TextPos { line: 0, col: 5 },
        dragging: false,
    };
    apply_selection_highlight(&mut buf, area, 1..2, &wrapped, &sel);

    for x in 0..30 {
        assert!(
            !buf[(x, 0)].modifier.contains(Modifier::REVERSED),
            "column {x} should not be highlighted — line 0 is scrolled off"
        );
    }
}

#[test]
fn apply_selection_highlight_preserves_existing_colors() {
    use crate::selection::{Selection, TextPos};
    let wrapped = vec![HistoryLine::plain(Line::from(Span::styled(
        "colored text",
        Style::default().fg(Color::Red),
    )))];
    let area = ratatui::layout::Rect::new(0, 0, 20, 5);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    render_history_into(&mut buf, area, &wrapped);

    let sel = Selection {
        anchor: TextPos { line: 0, col: 0 },
        cursor: TextPos { line: 0, col: 7 },
        dragging: false,
    };
    apply_selection_highlight(&mut buf, area, 0..1, &wrapped, &sel);

    assert_eq!(buf[(0, 0)].fg, Color::Red, "the red fg color must survive the highlight");
    assert!(buf[(0, 0)].modifier.contains(Modifier::REVERSED));
}
```

`HistoryLine::plain` is `pub(crate)`/private already (check `render.rs:410-416`) — since these tests live inside `render.rs`'s own `mod tests`, they can call it directly, same as existing tests do.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p polaris-tui render::tests::apply_selection_highlight -- --nocapture`
Expected: FAIL with "cannot find function `apply_selection_highlight`" (and `crate::selection` not being `pub` yet is fine — `render.rs` is in the same crate as the private `selection` module, so `crate::selection::Selection`/`TextPos` are reachable even though the module itself isn't `pub` outside the crate).

- [ ] **Step 3: Implement it**

Add near `render_history_into` (after it):

```rust
/// Overlays a selection's highlight onto an already-rendered `Buffer` —
/// call this immediately after `render_history_into` has drawn `lines`
/// into `area`. Only reverses video (`Modifier::REVERSED`); never touches
/// `fg`/`bg`, so existing colors (role color, `CODE_TEXT_COLOR`, shaded
/// rows) show through unchanged, just inverted — matching how ordinary
/// terminal selection highlighting looks. `wrapped`/`window` are the same
/// values `draw_frame` already computed for `render_history_into` itself.
pub fn apply_selection_highlight(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    window: std::ops::Range<usize>,
    wrapped: &[HistoryLine],
    sel: &crate::selection::Selection,
) {
    for (line_idx, start_col, end_col) in crate::selection::highlighted_columns(wrapped, sel) {
        if !window.contains(&line_idx) {
            continue;
        }
        let y_offset = (line_idx - window.start) as u16;
        if y_offset >= area.height {
            continue;
        }
        let row_y = area.y + y_offset;

        // start_col/end_col are character indices into the line's full
        // text; walk the same way `wrap_history_lines` does to turn them
        // into display columns (a full-width character occupies 2 cells).
        let Some(hl) = wrapped.get(line_idx) else {
            continue;
        };
        let mut char_idx = 0usize;
        let mut display_col: u16 = 0;
        'spans: for span in &hl.line.spans {
            for c in span.content.chars() {
                // `UnicodeWidthChar` is already imported unqualified at the
                // top of render.rs (see its `use unicode_width::{...}`) —
                // call `.width()` directly, matching `wrap_history_lines`'s
                // own style.
                let w = c.width().unwrap_or(0) as u16;
                if char_idx >= start_col && char_idx < end_col {
                    for dx in 0..w {
                        let x = area.x + display_col + dx;
                        if x >= area.x + area.width {
                            break 'spans;
                        }
                        buf[(x, row_y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
                    }
                }
                display_col += w;
                char_idx += 1;
                if char_idx >= end_col {
                    break 'spans;
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p polaris-tui render::tests -- --nocapture`
Expected: PASS, including the 3 new tests and every pre-existing `render.rs` test.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/render.rs
git commit -m "feat(polaris-tui): overlay a selection highlight onto the rendered buffer

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: `lib.rs` — `apply_selection_copy`

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs` (new private fn, near the existing `apply_copy_action` — grep for `fn apply_copy_action` to find it)

**Interfaces:**
- Consumes: nothing new (plain `&str` + injected closure).
- Produces:
  ```rust
  fn apply_selection_copy(text: &str, copy_fn: impl FnOnce(&str) -> Result<(), String>) -> String
  ```
  Task 8/11 call this on `Up`, passing `clipboard::copy_to_clipboard` as `copy_fn` in production and only calling it at all when `text` is non-empty (checked by the caller — this function assumes `text` is worth copying).

- [ ] **Step 1: Write the failing tests**

Add near the existing `apply_copy_action`-related tests (grep for `fn apply_copy_action_with_a_reply_wires_it_through` to find that neighborhood in `lib.rs`'s test module):

```rust
#[test]
fn apply_selection_copy_reports_success() {
    let status = apply_selection_copy("hello world", |text| {
        assert_eq!(text, "hello world");
        Ok(())
    });
    assert!(status.contains("copied"));
}

#[test]
fn apply_selection_copy_reports_a_failing_copy() {
    let status = apply_selection_copy("hello", |_| Err("no tty".to_string()));
    assert!(status.contains("can't copy"));
    assert!(status.contains("no tty"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p polaris-tui apply_selection_copy -- --nocapture`
Expected: FAIL with "cannot find function `apply_selection_copy`".

- [ ] **Step 3: Implement it**

Add next to `apply_copy_action`:

```rust
/// `Up`'s copy handler for mouse drag-selection, mirroring
/// `apply_copy_action`'s injectable-`copy_fn` pattern (see its own doc
/// comment for why: real clipboard I/O must never run unconditionally in
/// the test suite). Unlike `apply_copy_action`, this takes the text
/// directly rather than looking it up from a `Session` — the caller
/// (Tasks 8/11) already has it from `selection::extract_text`, and is
/// expected to only call this when `text` is non-empty (an empty/zero-
/// width selection copies nothing and shows no notice at all).
fn apply_selection_copy(text: &str, copy_fn: impl FnOnce(&str) -> Result<(), String>) -> String {
    match copy_fn(text) {
        Ok(()) => "copied selection to the clipboard".to_string(),
        Err(e) => format!("can't copy: {e}"),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p polaris-tui apply_selection_copy`
Expected: PASS, 2 tests.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

Note: `apply_selection_copy` isn't called from production code yet (Tasks 8/11 wire it up) — `cargo clippy --all-targets` still compiles the plain `lib` target without `cfg(test)`, so an unused-private-function warning is possible here since only the test module currently calls it in a way visible under `--all-targets`. If clippy reports `dead_code` on `apply_selection_copy` at this step, add `#[allow(dead_code)]` directly above the `fn` for now and remove it in Task 8 once real callers exist (matches this project's own established pattern for "written and tested but not yet wired in" — see the `cargo-clippy-all-targets-dead-code` skill if you want the full explanation).

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): add apply_selection_copy for mouse-selection Up

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: `lib.rs` — restore mouse capture + wheel-scroll

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:** none new — this restores exactly what commit `11adace` removed, so later tasks (8, 11) have `Event::Mouse` events to work with at all.

This task is a clean, mechanical revert of one specific prior change — it does **not** yet add selection handling (that's Task 8). Verify each edit against `git show 11adace -- crates/polaris-tui/src/lib.rs` if anything below is ambiguous — that commit's diff is the ground truth for exactly what was removed.

- [ ] **Step 1: Restore the `MOUSE_SCROLL_LINES` constant**

Find `const FOOTER_HEIGHT: u16 = ...` (grep for `FOOTER_HEIGHT` — the mouse-capture doc comment currently sits right after it, added when the constant was removed). Add back, right after `FOOTER_HEIGHT`:

```rust
/// How many `scroll_offset` lines one mouse/trackpad wheel tick moves —
/// a single line per tick feels sluggish for wheel input, unlike a key
/// press (see `PageUp`/`PageDown`'s own `+1`, which is a deliberate,
/// discrete step).
const MOUSE_SCROLL_LINES: usize = 3;
```

- [ ] **Step 2: Restore `EnableMouseCapture` at startup**

Grep for the doc comment beginning `// Deliberately never enables mouse capture` (added in commit `11adace`, currently sits right after `ratatui::init_with_options`). Replace that whole comment block with:

```rust
// Trackpad/mouse wheel scrolling of the conversation history, and mouse
// drag-to-select (see `selection.rs`) — without this, crossterm never
// emits `Event::Mouse` at all. This blocks the terminal's own native
// mouse handling (including drag-select) while polaris is running — see
// `selection.rs`'s module doc for why polaris implements its own
// selection instead of relying on that. Best-effort: a terminal that
// doesn't support mouse reporting just keeps not sending mouse events,
// same as before.
let _ = ratatui::crossterm::execute!(
    std::io::stdout(),
    ratatui::crossterm::event::EnableMouseCapture
);
```

- [ ] **Step 3: Restore `DisableMouseCapture` at shutdown**

Grep for `DisableUserShape`... actually grep for `SetCursorStyle::DefaultUserShape` — right after that `execute!` call, add back:

```rust
// Undoes the EnableMouseCapture set at startup — best-effort, same as
// the enable itself. Otherwise mouse reporting mode would leak into
// whatever the user's shell does next (e.g. text selection with the
// mouse would stop working until they open and close another
// mouse-reporting program).
let _ = ratatui::crossterm::execute!(
    std::io::stdout(),
    ratatui::crossterm::event::DisableMouseCapture
);
```

- [ ] **Step 4: Restore `ScrollUp`/`ScrollDown` handling in the idle loop**

Grep for `let ratatui::crossterm::event::Event::Key(key) = event else` in the idle loop (inside `'outer: loop`). Immediately before it, insert:

```rust
if let ratatui::crossterm::event::Event::Mouse(mouse) = &event {
    use ratatui::crossterm::event::MouseEventKind;
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            scroll_offset = scroll_offset.saturating_add(MOUSE_SCROLL_LINES);
        }
        MouseEventKind::ScrollDown => {
            scroll_offset = scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
        }
        _ => {}
    }
    continue;
}
```

(Task 8 replaces this `_ => {}` arm's contents with `Down`/`Drag`/`Up` handling — for this task, leave it as a no-op so the revert is isolated and testable on its own.)

- [ ] **Step 5: Restore `ScrollUp`/`ScrollDown` handling in the mid-turn loop**

Grep for `KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL)` inside the mid-turn `tokio::select!`'s `maybe_event = event_stream.next() =>` arm. That arm currently ends with:

```rust
Some(Ok(_)) => {}
Some(Err(_)) | None => break TurnOutcome::Fatal,
```

Replace `Some(Ok(_)) => {}` with:

```rust
Some(Ok(Event::Mouse(mouse))) => match mouse.kind {
    MouseEventKind::ScrollUp => {
        scroll_offset = scroll_offset.saturating_add(MOUSE_SCROLL_LINES);
    }
    MouseEventKind::ScrollDown => {
        scroll_offset = scroll_offset.saturating_sub(MOUSE_SCROLL_LINES);
    }
    _ => {}
},
Some(Ok(_)) => {}
```

This arm's `use` statement currently reads `use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};` — add `MouseEventKind` to that list.

- [ ] **Step 6: Full verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 7: Live tmux verification (wheel scroll only — selection isn't wired up yet)**

```bash
cargo build -p polaris-cli
TMPHOME=$(mktemp -d)
mkdir -p "$TMPHOME/.polaris"
printf '{"key":"sk-fake-demo-key"}' > "$TMPHOME/.polaris/api_key.json"
tmux new-session -d -s mousetest -x 100 -y 30 "env HOME=$TMPHOME POLARIS_PROVIDER=openai ./target/debug/polaris; sleep 60"
sleep 1
```

Send enough messages to have scrollable history (`tmux send-keys -t mousetest "test N" Enter` a handful of times), then send a scroll-wheel event via `tmux send-keys -t mousetest -- -X -N 3 scroll-up 2>/dev/null || true` — note: tmux's own copy-mode scroll bindings may intercept wheel events before they reach the child process depending on the user's tmux config; if a synthetic wheel event can't be driven through `tmux send-keys` in this environment, note that limitation explicitly rather than fabricating a pass, and fall back to confirming this in Task 12's final live verification pass instead (which happens after Task 11 makes the whole feature real-mouse-testable end to end, at which point a real physical mouse in a real terminal is worth using once instead of scripting synthetic events).

Clean up: `tmux send-keys -t mousetest C-c; tmux kill-session -t mousetest`.

- [ ] **Step 8: Commit**

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): restore mouse capture and wheel-scroll

Reverts the mouse-capture removal from commit 11adace — selection.rs
(added in prior tasks) replaces the native-terminal-selection dependency
that removal was protecting, so mouse capture (and the wheel-scroll it
enables) can come back. Selection handling itself lands in the next task.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 8: `lib.rs` — wire `Down`/`Drag`/`Up` selection handling into the idle loop

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `selection::{Selection, TextPos, text_pos_from_screen, extract_text}` (Tasks 1, 2, 4), `apply_selection_copy` (Task 6), `clipboard::copy_to_clipboard` (existing).
- Produces: `let mut selection: Option<selection::Selection>` in `run()`'s scope — Tasks 9, 10, 11 all read/write this same variable.

- [ ] **Step 1: Declare the selection state**

Grep for `let mut scroll_offset: usize = 0;` in `run()`. Immediately after it, add:

```rust
// Mouse drag-to-select over the conversation history — see
// `selection.rs`. `None` means no active or finalized selection.
let mut selection: Option<selection::Selection> = None;
```

- [ ] **Step 2: Wire `Down`/`Drag`/`Up` in the idle loop**

In the `if let ratatui::crossterm::event::Event::Mouse(mouse) = &event { ... }` block added in Task 7, replace the `_ => {}` arm with:

```rust
MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
    let history_area_height = area_height_for_history(FOOTER_HEIGHT, terminal_height(&terminal));
    if mouse.row < history_area_height {
        let pos = selection::text_pos_from_screen(
            wrapped_len_for_selection(&history, terminal_width(&terminal)),
            current_window(&history, scroll_offset, terminal_width(&terminal), history_area_height),
            ratatui::layout::Rect::new(0, 0, terminal_width(&terminal), history_area_height),
            mouse.row,
            mouse.column,
        );
        selection = Some(selection::Selection {
            anchor: pos,
            cursor: pos,
            dragging: true,
        });
    }
}
MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left) => {
    if let Some(sel) = selection.as_mut() {
        if sel.dragging {
            let history_area_height = area_height_for_history(FOOTER_HEIGHT, terminal_height(&terminal));
            let width = terminal_width(&terminal);
            let wrapped_len = wrapped_len_for_selection(&history, width);
            let window = current_window(&history, scroll_offset, width, history_area_height);
            sel.cursor = selection::text_pos_from_screen(
                wrapped_len,
                window,
                ratatui::layout::Rect::new(0, 0, width, history_area_height),
                mouse.row,
                mouse.column,
            );
            if mouse.row == 0 {
                scroll_offset = scroll_offset.saturating_add(1);
            } else if mouse.row + 1 >= history_area_height {
                scroll_offset = scroll_offset.saturating_sub(1);
            }
        }
    }
}
MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left) => {
    if let Some(sel) = selection.as_mut() {
        sel.dragging = false;
        let width = terminal_width(&terminal);
        let wrapped = render::wrap_history_lines(&history, width);
        let text = selection::extract_text(&wrapped, sel);
        if !text.is_empty() {
            status = Status::Notice(apply_selection_copy(&text, clipboard::copy_to_clipboard));
        }
    }
}
```

This references four small helpers that don't exist yet (`area_height_for_history`, `terminal_height`, `terminal_width`, `wrapped_len_for_selection`, `current_window`) — Step 3 below adds them. They exist purely to avoid repeating the same `terminal.borrow().size()` + `Layout::vertical` split + `wrap_history_lines` + `visible_history_window` sequence `draw_frame` already does, in three separate places (`Down`/`Drag`/`Up`) with slightly different subsets of it.

- [ ] **Step 3: Add the small helpers**

Add these near `draw_frame` (they factor out pieces `draw_frame` already computes inline, so a future refactor could have `draw_frame` itself call them — out of scope for this plan, but note the duplication in a doc comment so it's visible):

```rust
/// The terminal's current (width, height), or a fallback if the size
/// can't be read (matches the fallback other `run()` call sites already
/// use — see e.g. `abbreviate_home`'s caller).
fn terminal_size(terminal: &RefCell<ratatui::DefaultTerminal>) -> (u16, u16) {
    terminal
        .borrow()
        .size()
        .map(|s| (s.width, s.height))
        .unwrap_or((80, 24))
}

fn terminal_width(terminal: &RefCell<ratatui::DefaultTerminal>) -> u16 {
    terminal_size(terminal).0
}

fn terminal_height(terminal: &RefCell<ratatui::DefaultTerminal>) -> u16 {
    terminal_size(terminal).1
}

/// The history area's height, given the terminal's total height — mirrors
/// `draw_frame`'s own `Layout::vertical([Constraint::Min(0),
/// Constraint::Length(FOOTER_HEIGHT)])` split without needing a `Frame`
/// to do it (mouse-event handling runs outside the `draw` closure).
fn area_height_for_history(footer_height: u16, terminal_height: u16) -> u16 {
    terminal_height.saturating_sub(footer_height)
}

/// `wrap_history_lines(history, width).len()` — a thin name so call sites
/// above read as "the wrapped length for selection purposes" rather than
/// repeating the full `wrap_history_lines(...).len()` expression three
/// times.
fn wrapped_len_for_selection(history: &[render::HistoryLine], width: u16) -> usize {
    render::wrap_history_lines(history, width).len()
}

/// `visible_history_window`, given everything needed to reproduce exactly
/// what `draw_frame` used for the current frame.
fn current_window(
    history: &[render::HistoryLine],
    scroll_offset: usize,
    width: u16,
    history_area_height: u16,
) -> std::ops::Range<usize> {
    let wrapped_len = wrapped_len_for_selection(history, width);
    render::visible_history_window(wrapped_len, scroll_offset, history_area_height as usize)
}
```

- [ ] **Step 4: Remove the `#[allow(dead_code)]` from Task 6 if it was added**

If Task 6 added `#[allow(dead_code)]` above `apply_selection_copy`, remove it now — `Up`'s handling above calls it for real.

- [ ] **Step 5: Full verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 6: Live tmux verification**

```bash
cargo build -p polaris-cli
TMPHOME=$(mktemp -d)
mkdir -p "$TMPHOME/.polaris"
printf '{"key":"sk-fake-demo-key"}' > "$TMPHOME/.polaris/api_key.json"
tmux new-session -d -s dragtest -x 100 -y 30 "env HOME=$TMPHOME POLARIS_PROVIDER=openai ./target/debug/polaris; sleep 60"
sleep 1
```

Send a message so there's a visible line to select (`tmux send-keys -t dragtest "hello world" Enter`, wait for it to echo/fail against the fake key — the echoed user line itself is enough text to select even if the API call then fails). Then drive a drag with `tmux send-keys`'s mouse support if the tmux version supports synthetic mouse sequences in this environment; if it doesn't (many tmux builds only forward real mouse input, not synthetic `send-keys` mouse events), note that limitation explicitly and defer full drag verification to Task 12's real-terminal pass — do not fabricate a pass for something that couldn't actually be driven.

Clean up: `tmux send-keys -t dragtest C-c; tmux kill-session -t dragtest`.

- [ ] **Step 7: Commit**

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): wire mouse drag-selection into the idle loop

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 9: `render.rs`/`lib.rs` — draw the highlight in `draw_frame`

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs` (`draw_frame`)

**Interfaces:**
- Consumes: `render::apply_selection_highlight` (Task 5), `selection` (Task 8's `run()`-scope variable).
- Produces: `draw_frame` gains one new parameter, `selection: &Option<selection::Selection>` — every call site that builds a `draw_frame(...)` call (grep for `draw_frame(` — there are two: the idle loop's top-of-loop draw, and the mid-turn ticker's draw) must pass `&selection`.

- [ ] **Step 1: Add the parameter and the highlight call**

In `draw_frame`'s signature (grep for `fn draw_frame(`), add a new parameter after `selected_suggestion: usize`:

```rust
fn draw_frame(
    frame: &mut ratatui::Frame,
    history: &[render::HistoryLine],
    scroll_offset: usize,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &render::HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
    selection: &Option<selection::Selection>,
) {
```

The current code (confirmed, `lib.rs:256-259`) reads:

```rust
let wrapped = render::wrap_history_lines(history, history_area.width);
let window =
    render::visible_history_window(wrapped.len(), scroll_offset, history_area.height as usize);
render::render_history_into(frame.buffer_mut(), history_area, &wrapped[window]);
```

`&wrapped[window]` moves `window` (indexing a slice with a `Range<usize>` consumes it — `Range` isn't `Copy`). Change the last line to clone `window` before it's consumed, then add the highlight call right after:

```rust
render::render_history_into(frame.buffer_mut(), history_area, &wrapped[window.clone()]);
if let Some(sel) = selection {
    render::apply_selection_highlight(frame.buffer_mut(), history_area, window, &wrapped, sel);
}
```

(The second use of `window`, in `apply_selection_highlight`, can now move it for real since nothing after it needs `window` again.)

- [ ] **Step 2: Update both call sites**

Grep for `draw_frame(` (two production call sites — the idle loop's top-of-loop draw and the mid-turn ticker's draw; a third and fourth appear only in `#[cfg(test)]` code, updated in Step 3). Add `&selection` as the last argument to both.

- [ ] **Step 3: Update the existing `draw_frame` tests**

Grep for `fn draw_frame_puts_the_footer_in_the_bottom_inline_viewport_height_rows` and `fn draw_frame_wraps_a_history_line_wider_than_a_narrow_window_instead_of_clipping_it` — both call `draw_frame` directly and need `&None` appended as the new last argument (no selection active in those tests' scenarios).

- [ ] **Step 4: Write a new test for the highlight actually being drawn**

Add near the two tests found in Step 3:

```rust
#[test]
fn draw_frame_highlights_an_active_selection() {
    let backend = ratatui::backend::TestBackend::new(40, 8);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let history = vec![render::HistoryLine::plain(ratatui::text::Line::from(
        "select me",
    ))];
    let sel = Some(selection::Selection {
        anchor: selection::TextPos { line: 0, col: 0 },
        cursor: selection::TextPos { line: 0, col: 6 },
        dragging: false,
    });
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &history,
                0,
                "",
                0,
                &Status::Idle,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "gpt-5.4",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
                &sel,
            )
        })
        .expect("draw");
    let buf = terminal.backend().buffer();
    assert!(
        buf[(0, 0)].modifier.contains(ratatui::style::Modifier::REVERSED),
        "the first selected column should be highlighted"
    );
    assert!(
        !buf[(7, 0)].modifier.contains(ratatui::style::Modifier::REVERSED),
        "a column past the selection should not be highlighted"
    );
}
```

(Match the existing tests' exact pattern for building a `TestBackend`/`Terminal` and a minimal `HeaderInfo` — copy the surrounding test's setup if any field names above don't match what's actually there.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p polaris-tui draw_frame -- --nocapture`
Expected: PASS, all `draw_frame` tests including the new one.

- [ ] **Step 6: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): draw the selection highlight every frame

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 10: `lib.rs` — clear the selection on reset/resize/typing

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `reset_conversation_view` (existing, `lib.rs:181`) — gains one new parameter.

- [ ] **Step 1: Thread `selection` through `reset_conversation_view`**

Change its signature (`lib.rs:181`):

```rust
fn reset_conversation_view(
    history: &mut Vec<render::HistoryLine>,
    scroll_offset: &mut usize,
    selection: &mut Option<selection::Selection>,
    header: &render::HeaderInfo,
    width: u16,
) {
    history.clear();
    history.extend(render::header_history_lines(header, width));
    *scroll_offset = 0;
    *selection = None;
}
```

- [ ] **Step 2: Update every call site**

Run: `grep -n "reset_conversation_view(" crates/polaris-tui/src/lib.rs`

There are 8 production call sites (inside `run()`, split across the suggestion-popup-Enter path and the typed-command path — both `/new`/`/resume`/`/fork`/`/clear` handling). For each, add `&mut selection,` as the new second argument (right after `&mut scroll_offset,`). None of these call sites are inside `#[cfg(test)]` — the function's existing tests (if any call it directly) also need the new argument; grep for `reset_conversation_view(` inside `#[cfg(test)] mod tests` too and update those the same way.

- [ ] **Step 3: Clear the selection before `apply_key`**

Grep for `let text = if let Some(t) = review_text {` in the idle loop — this is the single point every ordinary keystroke (typing, backspace, enter, left/right) reaches, since Up/Down/PageUp/PageDown/Ctrl+O all `continue` before this point. Immediately before that line, add:

```rust
selection = None;
```

- [ ] **Step 4: Clear the selection on `Event::Resize`**

In the idle loop, right after the `if let ratatui::crossterm::event::Event::Mouse(mouse) = &event { ... }` block (Task 7/8), before `let ratatui::crossterm::event::Event::Key(key) = event else { continue; };`, add:

```rust
if let ratatui::crossterm::event::Event::Resize(_, _) = &event {
    selection = None;
}
```

- [ ] **Step 5: Full verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): clear the selection on reset, typing, and resize

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 11: `lib.rs` — wire selection handling into the mid-turn loop

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: everything from Tasks 6-8 (same functions/types, reused — no new signatures).
- Reuses: `mid_turn_notice_until`/`MID_TURN_NOTICE_DURATION` (existing, added in commit `11adace`) for the copy-confirmation `Notice`'s visibility, exactly the way `Ctrl+O`'s mid-turn handler already does — grep for `KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL)` inside the mid-turn loop to see the pattern to copy.

- [ ] **Step 1: Add `Down`/`Drag`/`Up` handling to the mid-turn loop's mouse arm**

In the `Some(Ok(Event::Mouse(mouse))) => match mouse.kind { ... }` arm added in Task 7 Step 5, replace its `_ => {}` with:

```rust
MouseEventKind::Down(MouseButton::Left) => {
    let history_area_height = area_height_for_history(FOOTER_HEIGHT, terminal_height(&terminal));
    if mouse.row < history_area_height {
        let width = terminal_width(&terminal);
        let wrapped_len = wrapped_len_for_selection(&history, width);
        let window = current_window(&history, scroll_offset, width, history_area_height);
        let pos = selection::text_pos_from_screen(
            wrapped_len,
            window,
            ratatui::layout::Rect::new(0, 0, width, history_area_height),
            mouse.row,
            mouse.column,
        );
        selection = Some(selection::Selection {
            anchor: pos,
            cursor: pos,
            dragging: true,
        });
    }
}
MouseEventKind::Drag(MouseButton::Left) => {
    if let Some(sel) = selection.as_mut() {
        if sel.dragging {
            let history_area_height = area_height_for_history(FOOTER_HEIGHT, terminal_height(&terminal));
            let width = terminal_width(&terminal);
            let wrapped_len = wrapped_len_for_selection(&history, width);
            let window = current_window(&history, scroll_offset, width, history_area_height);
            sel.cursor = selection::text_pos_from_screen(
                wrapped_len,
                window,
                ratatui::layout::Rect::new(0, 0, width, history_area_height),
                mouse.row,
                mouse.column,
            );
            if mouse.row == 0 {
                scroll_offset = scroll_offset.saturating_add(1);
            } else if mouse.row + 1 >= history_area_height {
                scroll_offset = scroll_offset.saturating_sub(1);
            }
        }
    }
}
MouseEventKind::Up(MouseButton::Left) => {
    if let Some(sel) = selection.as_mut() {
        sel.dragging = false;
        let width = terminal_width(&terminal);
        let wrapped = render::wrap_history_lines(&history, width);
        let text = selection::extract_text(&wrapped, sel);
        if !text.is_empty() {
            status = Status::Notice(apply_selection_copy(&text, clipboard::copy_to_clipboard));
            mid_turn_notice_until = Some(Instant::now() + MID_TURN_NOTICE_DURATION);
            if terminal
                .borrow_mut()
                .draw(|f| {
                    draw_frame(
                        f,
                        &history,
                        scroll_offset,
                        &input_buffer,
                        input_cursor,
                        &status,
                        &render::HeaderInfo {
                            provider_name: &args.provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        &[],
                        0,
                        &selection,
                    )
                })
                .is_err()
            {
                break TurnOutcome::Fatal;
            }
        }
    }
}
```

This adds `MouseButton` to the arm's `use` statement (already imports `Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind` from Task 7 — add `MouseButton` to that list).

The immediate `terminal.borrow_mut().draw(...)` call after a successful copy mirrors exactly what `Ctrl+O`'s mid-turn handler already does (see Task 6/the existing code found via the grep above) and for the same reason: nothing else in this loop redraws the footer between `tokio::select!` iterations except the 100ms ticker, which would otherwise clobber `status` back to `Thinking` before the confirmation is ever seen.

- [ ] **Step 2: Add `Event::Resize` handling to the mid-turn loop**

In the same `match maybe_event { ... }`, add a new arm right before `Some(Ok(_)) => {}`:

```rust
Some(Ok(Event::Resize(_, _))) => {
    selection = None;
}
```

- [ ] **Step 3: Full verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 4: Commit**

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): wire mouse drag-selection into the mid-turn loop

Reuses the existing mid_turn_notice_until mechanism (from commit 11adace)
so the copy confirmation survives the 100ms Thinking-status ticker the
same way Ctrl+O's mid-turn confirmation already does.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 12: Acceptance verification and live tmux/real-terminal check

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs` (only if verification below surfaces a bug — no planned production changes)
- No new production code expected — this task verifies, it doesn't implement.

The spec's requirements, and where each is covered:

1. Wheel-scroll works while typing/idle and mid-turn — Task 7 (restore) + Task 7 Step 7/this task's live check.
2. Drag selects, highlights, and auto-copies on release, in both idle and mid-turn states — Tasks 1-6 (unit-tested pure logic), Task 8 (idle wiring), Task 11 (mid-turn wiring), this task's live check (end-to-end, real mouse).
3. Selection clears on new click, typed input, `/new`/`/resume`/`/fork`/`/clear`, and resize — Task 10 (unit-untestable wiring, per this project's convention — see spec's テスト方針 §4), this task's live check.
4. Edge auto-scroll during drag — Tasks 8/11's `mouse.row == 0` / `mouse.row + 1 >= history_area_height` branches, this task's live check.
5. `/copy`/`Ctrl+O` unaffected — no task modifies `apply_copy_action`, `slash::Action::Copy`, or their tests; confirm with `git diff main --stat -- crates/polaris-tui/src/slash.rs` showing no changes in this plan's commits.
6. Only `crates/polaris-tui/` (and this plan/spec doc) touched — confirm with `git log --stat` across this plan's commits.

- [ ] **Step 1: Confirm scope (criteria 5, 6)**

Run: `git diff main --stat -- crates/polaris-tui/src/slash.rs`
Expected: empty output.

Run: `git log --oneline main..HEAD -- crates/polaris-tui docs/superpowers` then `git log --stat main..HEAD | grep -v "crates/polaris-tui\|docs/superpowers\|^commit\|^Author\|^Date\|^ \|^$"`
Expected: the second command's output is empty (nothing outside `crates/polaris-tui`/`docs/superpowers` was touched by this plan's own commits).

- [ ] **Step 2: Real-terminal manual verification**

This is the one part of this feature that genuinely can't be driven through `tmux send-keys` synthetic events in most environments (real mouse drag needs a real mouse) — do this in an actual terminal window (Terminal.app/iTerm2), not scripted:

```bash
cargo build -p polaris-cli
TMPHOME=$(mktemp -d)
mkdir -p "$TMPHOME/.polaris"
printf '{"key":"sk-fake-demo-key"}' > "$TMPHOME/.polaris/api_key.json"
env HOME="$TMPHOME" POLARIS_PROVIDER=openai ./target/debug/polaris
```

Then, by hand:
- Send a few messages so there's multi-line history to select from.
- Scroll with the mouse wheel — confirm the view moves.
- Click-drag across part of one line — confirm it highlights in reverse video as you drag, and releasing the mouse shows a "copied selection to the clipboard" notice. Paste somewhere else (`pbpaste`/another app) to confirm the OSC 52 copy actually reached the real clipboard.
- Drag across multiple lines — confirm the highlight spans full lines in between and the copied text has the expected `\n`-joined content.
- Start a drag, then move the mouse above the top edge or below the bottom edge of the history area while still holding the button — confirm the view auto-scrolls.
- Click once (no drag) elsewhere — confirm any previous highlight clears.
- With an active/finalized highlight visible, type a character into the input box — confirm the highlight clears.
- Run `/new`, `/resume`, `/fork`, and `/clear` (each once) with a highlight active beforehand — confirm each clears it.
- Resize the terminal window while a highlight is active — confirm it clears (documented limitation, not a bug).
- Send a message that takes a few seconds (or interrupt one mid-flight) and repeat the drag/select/copy steps while the turn is running — confirm the same behavior works mid-turn, and that the "copied" notice is visible for roughly a second and a half (not a sub-100ms flash) before the `Thinking` animation resumes.
- Confirm `Ctrl+O` and `/copy` still work exactly as before (unaffected by this feature).

Record pass/fail per bullet in this task's commit message or a report — this step has no automated assertion, so state the result explicitly.

- [ ] **Step 3: Full verification and final commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

If Step 2 surfaced a bug, fix it (with its own failing-test-first cycle if the fix is unit-testable; otherwise fix and re-verify live) as part of this task before committing.

```bash
git add crates/polaris-tui
git commit -m "test(polaris-tui): acceptance sweep for mouse drag-selection

Manual live-terminal verification recorded in the task report: wheel
scroll, drag-select-and-auto-copy (idle and mid-turn), edge auto-scroll,
clearing on click/typing/reset/resize, Ctrl+O/\`/copy\` unaffected.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:** §1 (`selection.rs` types/functions) → Tasks 1-4. §2 (selection state + clearing) → Task 8 Step 1 (state) + Task 10 (clearing). §3 (rendering) → Task 5 (function) + Task 9 (wiring). §4 (edge auto-scroll) → Tasks 8/11's `Drag` handling. §5 (mouse capture/wheel-scroll restoration) → Task 7. テスト方針 1-3 → Tasks 1-6's TDD steps. テスト方針 4 → explicitly deferred to live verification per the spec's own words, honored in Tasks 7/8/12. テスト方針 5 → Task 12 Step 2. 既知の制約 (resize clears selection, event-driven auto-scroll, no block selection) → Task 10 Step 4, Tasks 8/11's `Drag` handling, and no rectangular-selection code exists anywhere in this plan.

**Placeholder scan:** every step has real code or an exact command; the two `tmux send-keys` mouse-event caveats (Tasks 7/8) are honest statements of a real environment limitation with an explicit fallback (defer to Task 12's real-terminal pass), not a skipped verification.

**Type consistency:** `TextPos { line, col }` (Task 1) is used identically by every later task. `text_pos_from_screen(wrapped_len: usize, window: Range<usize>, area: Rect, screen_row: u16, screen_col: u16) -> TextPos` (Task 2) matches its call sites in Tasks 8/11 exactly (same argument order and types). `highlighted_columns(wrapped: &[HistoryLine], sel: &Selection) -> Vec<(usize, usize, usize)>` (Task 3) is consumed identically by `extract_text` (Task 4) and `apply_selection_highlight` (Task 5). `apply_selection_highlight(buf, area, window, wrapped, sel)` (Task 5) matches its one call site in `draw_frame` (Task 9). `apply_selection_copy(text: &str, copy_fn) -> String` (Task 6) matches its two call sites (Tasks 8, 11) exactly. `reset_conversation_view`'s new `selection` parameter (Task 10) is inserted at the same position (second, right after `scroll_offset`) at every one of its 8 call sites.
