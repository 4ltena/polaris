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
        let a = (self.anchor.line, self.anchor.col);
        let b = (self.cursor.line, self.cursor.col);
        if a <= b {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// Resolves a display-column offset (`0` = the line's leftmost cell) to
/// the character index it falls within, walking `hl`'s `Span`s and
/// accumulating each character's `UnicodeWidthChar` display width — the
/// same width-accumulation walk `render::apply_selection_highlight` does
/// in the other direction (char index -> display column), just inverted.
/// A full-width character occupies two consecutive display columns; a
/// `display_col` landing on either of them resolves to that one character,
/// not the next. A `display_col` at or past the line's total display
/// width clamps to `char_count(hl)` (one past the last character), so
/// "select to the end of the line" callers get a valid, usable index
/// rather than an out-of-range one.
fn char_index_for_display_col(hl: &crate::render::HistoryLine, display_col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;

    let mut char_idx = 0usize;
    let mut col = 0usize;
    for span in &hl.line.spans {
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if display_col < col + w {
                return char_idx;
            }
            col += w;
            char_idx += 1;
        }
    }
    char_idx
}

/// Converts a screen coordinate (as carried by `crossterm`'s
/// `MouseEvent::{row,column}`) into a `TextPos` in the `wrapped` array's
/// coordinate space. Clamps to the nearest valid line/column rather than
/// returning `Option` — a drag that strays outside `area` (common: the
/// user's mouse leaves the history region while still holding the
/// button) should extend the selection to the nearest edge, not silently
/// stop updating.
///
/// `screen_col - area.x` is a *display-column* offset, not a character
/// index — a raw offset can land in the middle of a wide/full-width
/// character, and full-width text has more display columns than
/// characters. This resolves it against the actual line it lands on (via
/// `char_index_for_display_col`) before returning, so `TextPos::col` is
/// always a true character index — callers (`highlighted_columns`,
/// `extract_text`) can use it directly with no further conversion.
pub fn text_pos_from_screen(
    wrapped: &[crate::render::HistoryLine],
    window: std::ops::Range<usize>,
    area: ratatui::layout::Rect,
    screen_row: u16,
    screen_col: u16,
) -> TextPos {
    if wrapped.is_empty() {
        return TextPos { line: 0, col: 0 };
    }

    let row_in_area = screen_row.saturating_sub(area.y);
    let max_row_in_area = area.height.saturating_sub(1);
    let clamped_row = row_in_area.min(max_row_in_area);
    let line = (window.start + clamped_row as usize).min(wrapped.len().saturating_sub(1));

    let display_col = screen_col.saturating_sub(area.x) as usize;
    let col = wrapped
        .get(line)
        .map(|hl| char_index_for_display_col(hl, display_col))
        .unwrap_or(0);

    TextPos { line, col }
}

/// Character count of a `HistoryLine`'s full rendered text (all spans
/// concatenated) — used to clamp/compute "to the end of this line"
/// without needing to know display width here (`Modifier::REVERSED` is
/// applied per-cell downstream in `apply_selection_highlight`, which is
/// where width actually matters).
fn char_count(hl: &crate::render::HistoryLine) -> usize {
    hl.line
        .spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum()
}

/// Turns a `Selection` into per-line `(line, start_col, end_col)` ranges
/// (character indices, `end_col` exclusive) over `wrapped`. Pure logic —
/// no rendering; Task 5's `apply_selection_highlight` walks each line's
/// spans the same width-aware way `wrap_history_lines` already does to
/// turn these character indices into screen columns.
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

/// The full rendered text of one `HistoryLine` (all its spans
/// concatenated) — `char_count`'s sibling, needed here (unlike in
/// `highlighted_columns`) because this function returns text, not just
/// counts.
fn line_text(hl: &crate::render::HistoryLine) -> String {
    hl.line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Extracts the selected text as a single `String`, joining lines with
/// `"\n"`. Reuses `highlighted_columns` directly rather than re-deriving
/// the same line/column ranges — `Selection` normalization and per-line
/// clamping live in exactly one place.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::HistoryLine;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    fn hl(text: &str) -> HistoryLine {
        HistoryLine {
            line: Line::from(text.to_string()),
            shaded: false,
        }
    }

    fn sel(anchor: (usize, usize), cursor: (usize, usize)) -> Selection {
        Selection {
            anchor: TextPos {
                line: anchor.0,
                col: anchor.1,
            },
            cursor: TextPos {
                line: cursor.0,
                col: cursor.1,
            },
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

    #[test]
    fn an_end_line_past_the_end_of_wrapped_does_not_panic_and_clamps() {
        let wrapped = vec![hl("only line")];
        // end.line (5) is past wrapped.len() (1) -- e.g. history shrank
        // (a `/clear`) mid-drag while a stale Selection still points past
        // the end.
        let got = highlighted_columns(&wrapped, &sel((0, 2), (5, 3)));
        assert_eq!(got, vec![(0, 2, 9)]);
    }

    #[test]
    fn an_empty_wrapped_slice_highlights_nothing() {
        let wrapped: Vec<HistoryLine> = Vec::new();
        let got = highlighted_columns(&wrapped, &sel((0, 0), (2, 3)));
        assert!(got.is_empty());
    }

    fn pos(line: usize, col: usize) -> TextPos {
        TextPos { line, col }
    }

    fn area(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
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

    /// `n` ASCII-only `HistoryLine`s (single-width chars throughout), for
    /// tests that only care about which `line` a row maps to.
    fn ascii_lines(n: usize) -> Vec<HistoryLine> {
        (0..n).map(|_| hl("row content here")).collect()
    }

    #[test]
    fn a_point_inside_the_area_maps_to_the_matching_wrapped_line() {
        // area starts at (0, 0), 10 rows tall; window shows wrapped[0..10].
        // row 3 -> wrapped[3]; col is left as-is (col-mapping tested separately below).
        let wrapped = ascii_lines(10);
        let pos = text_pos_from_screen(&wrapped, 0..10, area(0, 0, 40, 10), 3, 0);
        assert_eq!(pos.line, 3);
    }

    #[test]
    fn the_window_start_offsets_which_wrapped_line_a_row_maps_to() {
        // window is 20..30 (scrolled back) -> row 0 is wrapped[20], row 3 is wrapped[23].
        let wrapped = ascii_lines(30);
        let pos = text_pos_from_screen(&wrapped, 20..30, area(0, 0, 40, 10), 3, 0);
        assert_eq!(pos.line, 23);
    }

    #[test]
    fn an_area_offset_from_the_screen_origin_is_subtracted_first() {
        // area starts at y=5 (e.g. below a header) -> screen_row=7 is the area's row 2.
        let wrapped = ascii_lines(10);
        let pos = text_pos_from_screen(&wrapped, 0..10, area(0, 5, 40, 10), 7, 0);
        assert_eq!(pos.line, 2);
    }

    #[test]
    fn a_row_past_the_area_or_window_clamps_to_the_last_visible_line() {
        let wrapped = ascii_lines(10);
        let pos = text_pos_from_screen(&wrapped, 0..10, area(0, 0, 40, 5), 50, 0);
        // area is only 5 rows tall (rows 0..5 -> wrapped[0..5]); clamp to the last, wrapped[4].
        assert_eq!(pos.line, 4);
    }

    #[test]
    fn a_row_above_the_area_clamps_to_the_first_visible_line() {
        let wrapped = ascii_lines(10);
        let pos = text_pos_from_screen(&wrapped, 3..8, area(0, 10, 40, 5), 2, 0);
        // screen_row (2) is above area.y (10) -> clamp to the window's first line.
        assert_eq!(pos.line, 3);
    }

    #[test]
    fn an_empty_wrapped_array_clamps_to_line_zero() {
        let wrapped: Vec<HistoryLine> = Vec::new();
        let pos = text_pos_from_screen(&wrapped, 0..0, area(0, 0, 40, 10), 3, 0);
        assert_eq!(pos.line, 0);
    }

    #[test]
    fn column_zero_maps_to_char_index_zero() {
        let wrapped = vec![hl("hello")];
        let pos = text_pos_from_screen(&wrapped, 0..1, area(2, 0, 40, 10), 0, 2);
        // area.x = 2, screen_col = 2 -> display-col-within-area = 0 -> char index 0.
        assert_eq!(pos.col, 0);
    }

    #[test]
    fn a_column_before_the_area_clamps_to_zero() {
        let wrapped = vec![hl("hello")];
        let pos = text_pos_from_screen(&wrapped, 0..1, area(5, 0, 40, 10), 0, 1);
        assert_eq!(pos.col, 0);
    }

    #[test]
    fn a_full_width_display_column_maps_to_the_character_covering_it() {
        // "あいうえお" is 5 full-width characters, 10 display columns:
        // あ(cols 0-1) い(2-3) う(4-5) え(6-7) お(8-9).
        let wrapped = vec![hl("あいうえお")];
        // Display column 0 -> あ (char index 0).
        let start = text_pos_from_screen(&wrapped, 0..1, area(0, 0, 40, 1), 0, 0);
        assert_eq!(start.col, 0);
        // Display column 5 lands inside う's two-cell span (cols 4-5) -> char index 2.
        let mid = text_pos_from_screen(&wrapped, 0..1, area(0, 0, 40, 1), 0, 5);
        assert_eq!(mid.col, 2);
        // Selecting from display column 0 to display column 5 must therefore
        // select characters 0..2 ("あい"), not "all 5 characters" (the old,
        // uncorrected display-column-as-char-index bug) and not zero
        // characters either.
        let sel = Selection {
            anchor: start,
            cursor: mid,
            dragging: false,
        };
        assert_eq!(extract_text(&wrapped, &sel), "あい");
    }

    #[test]
    fn a_display_column_after_full_width_text_resolves_the_ascii_tail_correctly() {
        // "あいうえお" (10 display cols, chars 0-4) followed by "hello"
        // (chars 5-9, one display col each) -- covers the finding's other
        // half: dragging into the ASCII text *after* full-width text must
        // not clamp both endpoints to char_count and produce an empty
        // selection.
        let wrapped = vec![hl("あいうえおhello")];
        // Display column 10 is the first cell of "h" -> char index 5.
        let from = text_pos_from_screen(&wrapped, 0..1, area(0, 0, 40, 1), 0, 10);
        assert_eq!(from.col, 5);
        // Display column 14 is "o" -> char index 9.
        let to = text_pos_from_screen(&wrapped, 0..1, area(0, 0, 40, 1), 0, 14);
        assert_eq!(to.col, 9);
        let sel = Selection {
            anchor: from,
            cursor: to,
            dragging: false,
        };
        assert_eq!(extract_text(&wrapped, &sel), "hell");
    }

    #[test]
    fn a_display_column_past_the_lines_width_clamps_to_char_count() {
        let wrapped = vec![hl("あい")]; // 2 chars, 4 display columns
        let pos = text_pos_from_screen(&wrapped, 0..1, area(0, 0, 40, 1), 0, 99);
        assert_eq!(pos.col, 2);
    }

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
