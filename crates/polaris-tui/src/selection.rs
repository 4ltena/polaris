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
