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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

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
}
