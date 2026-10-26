//! Selection of visible terminal cells only; no input buffers or persistence.
use ratatui::{
    buffer::Buffer,
    crossterm::event::{
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::Position,
    style::Modifier,
};
use unicode_width::UnicodeWidthStr;

pub(crate) fn link_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

#[derive(Default)]
pub(crate) struct LinkClick {
    pressed: Option<Position>,
}
impl LinkClick {
    pub(crate) fn cancel(&mut self) {
        self.pressed = None;
    }
    pub(crate) fn mouse(&mut self, mouse: MouseEvent, hit: bool) -> bool {
        let point = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed = (hit && link_modifier(mouse.modifiers)).then_some(point);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                return self
                    .pressed
                    .take()
                    .is_some_and(|pressed| pressed == point && hit);
            }
            MouseEventKind::Moved => {}
            _ => self.cancel(),
        }
        false
    }
}

pub(crate) enum Effect {
    Pass,
    Paint,
    Copy(String),
}
#[derive(Default)]
pub(crate) struct Selection {
    anchor: Option<Position>,
    cursor: Option<Position>,
    dragging: bool,
    link_gesture: bool,
}
impl Selection {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
    fn point(buffer: &Buffer, x: u16, y: u16) -> Position {
        Position::new(
            x.clamp(buffer.area.x, buffer.area.right()),
            y.clamp(buffer.area.y, buffer.area.bottom().saturating_sub(1)),
        )
    }
    pub(crate) fn event(&mut self, event: &Event, buffer: &Buffer) -> Effect {
        if buffer.area.is_empty() {
            return Effect::Pass;
        }
        match event {
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) if link_modifier(mouse.modifiers) => {
                    self.clear();
                    self.link_gesture = true;
                    Effect::Pass
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.clear();
                    if buffer.area.contains((mouse.column, mouse.row).into()) {
                        let point = Self::point(buffer, mouse.column, mouse.row);
                        self.anchor = Some(point);
                        self.cursor = Some(point);
                        self.dragging = true;
                    }
                    Effect::Paint
                }
                MouseEventKind::Drag(MouseButton::Left) if self.link_gesture => Effect::Pass,
                MouseEventKind::Up(MouseButton::Left) if self.link_gesture => {
                    self.link_gesture = false;
                    Effect::Pass
                }
                MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                    self.cursor = Some(Self::point(buffer, mouse.column, mouse.row));
                    Effect::Paint
                }
                MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                    self.cursor = Some(Self::point(buffer, mouse.column, mouse.row));
                    self.dragging = false;
                    let text = self.text(buffer);
                    if text.is_empty() {
                        Effect::Paint
                    } else {
                        Effect::Copy(text)
                    }
                }
                MouseEventKind::Moved => Effect::Pass,
                _ => {
                    self.clear();
                    Effect::Pass
                }
            },
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && matches!(key.code, KeyCode::Char('c' | 'C'))
                    && (key.modifiers.contains(KeyModifiers::SUPER)
                        || key
                            .modifiers
                            .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)) =>
            {
                let text = self.text(buffer);
                if text.is_empty() {
                    Effect::Paint
                } else {
                    Effect::Copy(text)
                }
            }
            Event::Key(_) | Event::Paste(_) | Event::Resize(_, _) | Event::FocusLost => {
                self.clear();
                Effect::Pass
            }
            _ => Effect::Pass,
        }
    }
    fn columns(&self, buffer: &Buffer, row: u16) -> Option<(u16, u16)> {
        let (a, b) = (self.anchor?, self.cursor?);
        let (start, end) = if (a.y, a.x) <= (b.y, b.x) {
            (a, b)
        } else {
            (b, a)
        };
        if start == end || row < start.y || row > end.y {
            return None;
        }
        let left = if row == start.y {
            start.x
        } else {
            buffer.area.x
        };
        let right = if row == end.y {
            end.x
        } else {
            buffer.area.right()
        };
        (left < right).then_some((left, right))
    }
    pub(crate) fn highlight(&self, buffer: &mut Buffer) {
        for y in buffer.area.y..buffer.area.bottom() {
            let Some((left, right)) = self.columns(buffer, y) else {
                continue;
            };
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let width = buffer[(x, y)]
                    .symbol()
                    .width()
                    .max(1)
                    .min(usize::from(u16::MAX)) as u16;
                let next = x.saturating_add(width).min(buffer.area.right());
                if x < right && next > left {
                    for column in x..next {
                        buffer[(column, y)].modifier ^= Modifier::REVERSED;
                    }
                }
                x = next;
            }
        }
    }
    pub(crate) fn dragging(&self) -> bool {
        self.dragging
    }
    pub(crate) fn text(&self, buffer: &Buffer) -> String {
        let mut rows = Vec::new();
        for y in buffer.area.y..buffer.area.bottom() {
            let Some((left, right)) = self.columns(buffer, y) else {
                continue;
            };
            let mut row = String::new();
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let cell = &buffer[(x, y)];
                let width = cell.symbol().width().max(1).min(usize::from(u16::MAX)) as u16;
                let next = x.saturating_add(width).min(buffer.area.right());
                if x < right && next > left {
                    if cell.modifier.contains(Modifier::HIDDEN) {
                        row.extend(std::iter::repeat_n(' ', usize::from(next - x)));
                    } else {
                        row.push_str(cell.symbol());
                    }
                }
                x = next;
            }
            rows.push(row);
        }
        if rows.len() > 1 {
            for row in &mut rows {
                row.truncate(row.trim_end_matches(' ').len());
            }
        }
        rows.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Cell, layout::Rect};
    fn mouse(kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers,
        })
    }
    #[test]
    fn visible_copy_preserves_wide_combining_and_reverse_ranges_but_not_hidden_values() {
        let mut b = Buffer::empty(Rect::new(0, 0, 12, 3));
        b.set_string(0, 0, "A日e\u{301}B", ratatui::style::Style::default());
        b[(0, 1)] = Cell::new("hidden-canary")
            .set_style(ratatui::style::Style::default().add_modifier(Modifier::HIDDEN))
            .clone();
        b.set_string(0, 2, "last", ratatui::style::Style::default());
        for (start, end) in [((0, 0), (5, 0)), ((5, 0), (0, 0))] {
            let mut s = Selection::default();
            assert!(matches!(
                s.event(
                    &mouse(
                        MouseEventKind::Down(MouseButton::Left),
                        start.0,
                        start.1,
                        KeyModifiers::NONE
                    ),
                    &b
                ),
                Effect::Paint
            ));
            let Effect::Copy(text) = s.event(
                &mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    end.0,
                    end.1,
                    KeyModifiers::NONE,
                ),
                &b,
            ) else {
                panic!("no copy")
            };
            assert_eq!(text, "A日e\u{301}B");
        }
        let mut s = Selection::default();
        s.event(
            &mouse(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE,
            ),
            &b,
        );
        let Effect::Copy(text) = s.event(
            &mouse(
                MouseEventKind::Up(MouseButton::Left),
                4,
                2,
                KeyModifiers::NONE,
            ),
            &b,
        ) else {
            panic!("no multiline copy")
        };
        assert_eq!(text, "A日e\u{301}B\n\nlast");
        assert!(!text.contains("hidden-canary"));
    }
    #[test]
    fn modifier_click_is_up_only_single_shot_and_drag_cancels() {
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::SUPER] {
            let mut link = LinkClick::default();
            let Event::Mouse(down) =
                mouse(MouseEventKind::Down(MouseButton::Left), 2, 3, modifiers)
            else {
                unreachable!()
            };
            let Event::Mouse(up) = mouse(MouseEventKind::Up(MouseButton::Left), 2, 3, modifiers)
            else {
                unreachable!()
            };
            assert!(!link.mouse(down, true));
            assert!(link.mouse(up, true));
            assert!(!link.mouse(up, true));
            assert!(!link.mouse(down, true));
            let Event::Mouse(drag) =
                mouse(MouseEventKind::Drag(MouseButton::Left), 3, 3, modifiers)
            else {
                unreachable!()
            };
            assert!(!link.mouse(drag, true));
            assert!(!link.mouse(up, true));
        }
        let mut link = LinkClick::default();
        let Event::Mouse(down) = mouse(
            MouseEventKind::Down(MouseButton::Left),
            2,
            3,
            KeyModifiers::NONE,
        ) else {
            unreachable!()
        };
        assert!(!link.mouse(down, true));
        let Event::Mouse(up) = mouse(
            MouseEventKind::Up(MouseButton::Left),
            2,
            3,
            KeyModifiers::NONE,
        ) else {
            unreachable!()
        };
        assert!(!link.mouse(up, true));
    }
}
