//! Reveal an explicitly clicked local file in Finder. No file body is opened.

use crate::{render, selection};
use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Pressed {
    point: Position,
    area: Rect,
    scroll: usize,
    path: PathBuf,
}

#[derive(Default)]
pub(super) struct FileClick {
    pressed: Option<Pressed>,
}

impl FileClick {
    pub(super) fn mouse(
        &mut self,
        mouse: MouseEvent,
        history: &[render::HistoryLine],
        cwd: &Path,
        area: Rect,
        scroll: usize,
    ) -> Option<PathBuf> {
        let point = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.press(history, cwd, area, scroll, point)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                return self.release(history, cwd, area, scroll, point);
            }
            MouseEventKind::Moved => {}
            _ => self.cancel(),
        }
        None
    }

    pub(super) fn cancel(&mut self) {
        self.pressed = None;
    }

    pub(super) fn press(
        &mut self,
        history: &[render::HistoryLine],
        cwd: &Path,
        area: Rect,
        scroll: usize,
        point: Position,
    ) {
        self.pressed = path_at(history, cwd, area, scroll, point).map(|path| Pressed {
            point,
            area,
            scroll,
            path,
        });
    }

    pub(super) fn release(
        &mut self,
        history: &[render::HistoryLine],
        cwd: &Path,
        area: Rect,
        scroll: usize,
        point: Position,
    ) -> Option<PathBuf> {
        let pressed = self.pressed.take()?;
        if pressed.point != point || pressed.area != area || pressed.scroll != scroll {
            return None;
        }
        let current = path_at(history, cwd, area, scroll, point)?;
        (current == pressed.path).then_some(current)
    }
}

fn path_at(
    history: &[render::HistoryLine],
    cwd: &Path,
    area: Rect,
    scroll: usize,
    point: Position,
) -> Option<PathBuf> {
    if !area.contains(point) {
        return None;
    }
    let wrapped = render::wrap_history_lines(history, area.width);
    let window = render::visible_history_window(wrapped.len(), scroll, area.height as usize);
    // Selection clamps empty cells to nearby text; a click must hit an actual
    // visible character instead of activating that adjacent file.
    let row = window.start + usize::from(point.y - area.y);
    if row >= window.end {
        return None;
    }
    let position = selection::text_pos_from_screen(&wrapped, window, area, point.y, point.x);
    let row_text = wrapped.get(row)?.line.to_string();
    let clicked_byte = row_text.char_indices().nth(position.col)?.0;

    let mut first_row = 0;
    for original in history {
        let rows = render::wrap_history_lines(std::slice::from_ref(original), area.width);
        if row >= first_row + rows.len() {
            first_row += rows.len();
            continue;
        }
        if original.shaded {
            return None;
        }
        let text = original.line.to_string();
        let mut offset = 0;
        for (index, rendered) in rows.iter().enumerate() {
            let segment = rendered.line.to_string();
            let start = offset + text.get(offset..)?.find(&segment)?;
            if index == row - first_row {
                return path_in_line(original, &text, start + clicked_byte, cwd);
            }
            // Reflow can drop spaces at a row boundary. Locate each retained
            // segment in order rather than treating wrapped rows as raw text.
            offset = start + segment.len();
        }
        return None;
    }
    None
}

fn path_in_line(
    line: &render::HistoryLine,
    text: &str,
    clicked: usize,
    cwd: &Path,
) -> Option<PathBuf> {
    // The visible label of a Markdown link resolves to its explicit local
    // destination, including <paths with spaces>. URLs are never launched.
    let mut cursor = 0;
    while let Some(separator) = text[cursor..].find("](") {
        let separator = cursor + separator;
        let Some(open) = text[..separator].rfind('[') else {
            cursor = separator + 2;
            continue;
        };
        let Some(close) = text[separator + 2..].find(')') else {
            break;
        };
        let close = separator + 2 + close;
        if (open..=close).contains(&clicked) {
            return local_file(&text[separator + 2..close], cwd);
        }
        cursor = close + 1;
    }

    // Inline-code spans preserve filenames containing spaces after the
    // renderer removes their backticks. Quoted tool paths are handled too.
    let mut start = 0;
    for span in &line.line.spans {
        let end = start + span.content.len();
        if (start..end).contains(&clicked)
            && let Some(path) = local_file(&span.content, cwd)
        {
            return Some(path);
        }
        start = end;
    }
    for (open, close) in [('"', '"'), ('\'', '\''), ('(', ')')] {
        if let Some(start) = text[..clicked].rfind(open)
            && let Some(end) = text[clicked..].find(close)
            && let Some(path) = local_file(&text[start + open.len_utf8()..clicked + end], cwd)
        {
            return Some(path);
        }
    }
    let delimiter = |c: char| c.is_whitespace() || "`\"'()[]<>,;「」『』、。".contains(c);
    let start = text[..clicked]
        .char_indices()
        .rfind(|(_, c)| delimiter(*c))
        .map_or(0, |(index, c)| index + c.len_utf8());
    let end = text[clicked..]
        .find(delimiter)
        .map_or(text.len(), |offset| clicked + offset);
    if start >= end {
        return None;
    }
    local_file(&text[start..end], cwd)
        .or_else(|| local_file(text[start..end].trim_end_matches(['.', ':']), cwd))
}

fn local_file(value: &str, cwd: &Path) -> Option<PathBuf> {
    let value = value.trim().trim_start_matches('<').trim_end_matches('>');
    if value.is_empty()
        || value.len() > 4096
        || value.contains("://")
        || value.chars().any(char::is_control)
    {
        return None;
    }
    let resolve = |value: &str| {
        let path = if let Some(tail) = value.strip_prefix("~/") {
            PathBuf::from(std::env::var_os("HOME")?).join(tail)
        } else {
            cwd.join(value)
        };
        let path = path.canonicalize().ok()?;
        path.is_file().then_some(path)
    };
    if let Some(path) = resolve(value) {
        return Some(path);
    }
    let mut without_position = value;
    for _ in 0..2 {
        let (path, position) = without_position.rsplit_once(':')?;
        if position.is_empty() || !position.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if let Some(path) = resolve(path) {
            return Some(path);
        }
        without_position = path;
    }
    None
}

fn command(path: &Path) -> Command {
    let mut command = Command::new("/usr/bin/open");
    command
        .arg("-R")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub(super) fn reveal(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !path.is_file() {
        return Err("Cannot verify the clicked file".into());
    }
    match command(path).status() {
        Ok(status) if status.success() => Ok(()),
        _ => Err("Failed to request reveal in Finder".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::Message;

    fn click(text: &str, name: &str, width: u16, root: &Path) -> Option<PathBuf> {
        let history = render::history_lines_for(&[Message::assistant(text)]);
        let wrapped = render::wrap_history_lines(&history, width);
        let (row, line) = wrapped
            .iter()
            .enumerate()
            .find(|(_, line)| line.line.to_string().contains(name))
            .unwrap();
        let text = line.line.to_string();
        let offset = text.find(name).unwrap();
        use unicode_width::UnicodeWidthStr;
        path_at(
            &history,
            root,
            Rect::new(0, 0, width, 100),
            0,
            Position::new(text[..offset].width() as u16, row as u16),
        )
    }

    #[test]
    fn resolves_visible_unicode_code_spans_links_and_line_references() {
        let root = tempfile::tempdir().unwrap();
        for name in ["README.md", "日本語の資料.txt", "My Report.md"] {
            std::fs::write(root.path().join(name), "synthetic").unwrap();
        }
        let readme = root.path().join("README.md").canonicalize().unwrap();
        assert_eq!(
            click("確認: README.md:12:3", "README", 80, root.path()),
            Some(readme.clone())
        );
        assert_eq!(
            click("[説明](README.md:12)", "説明", 80, root.path()),
            Some(readme)
        );
        for name in ["日本語の資料.txt", "My Report.md"] {
            assert_eq!(
                click(&format!("確認: `{name}`"), name, 80, root.path()),
                Some(root.path().join(name).canonicalize().unwrap())
            );
        }
        assert_eq!(
            click("[資料](<My Report.md>)", "資料", 80, root.path()),
            Some(root.path().join("My Report.md").canonicalize().unwrap())
        );
    }

    #[test]
    fn resolves_a_filename_across_hard_wrapping_and_after_scrolling() {
        let root = tempfile::tempdir().unwrap();
        let name = "long_filename_that_wraps.txt";
        std::fs::write(root.path().join(name), "synthetic").unwrap();
        assert_eq!(
            click(name, "wraps", 12, root.path()),
            Some(root.path().join(name).canonicalize().unwrap())
        );
        let history = render::history_lines_for(&[Message::assistant("README.md\nsecond\nthird")]);
        std::fs::write(root.path().join("README.md"), "synthetic").unwrap();
        assert_eq!(
            path_at(
                &history,
                root.path(),
                Rect::new(0, 0, 80, 1),
                2,
                Position::new(10, 0)
            ),
            Some(root.path().join("README.md").canonicalize().unwrap())
        );
    }

    #[test]
    fn click_requires_matching_press_release_and_drag_cancellation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "synthetic").unwrap();
        let history = render::history_lines_for(&[Message::assistant("README.md")]);
        let area = Rect::new(0, 0, 80, 10);
        let point = Position::new(10, 0);
        let mut gesture = FileClick::default();
        gesture.press(&history, root.path(), area, 0, point);
        assert!(
            gesture
                .release(&history, root.path(), area, 0, point)
                .is_some()
        );
        gesture.press(&history, root.path(), area, 0, point);
        gesture.mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: point.x,
                row: point.y,
                modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
            },
            &history,
            root.path(),
            area,
            0,
        );
        assert!(
            gesture
                .release(&history, root.path(), area, 0, point)
                .is_none()
        );
        gesture.press(&history, root.path(), area, 0, point);
        assert!(
            gesture
                .release(&history, root.path(), Rect::new(0, 0, 40, 10), 0, point)
                .is_none()
        );
        gesture.press(&history, root.path(), area, 0, point);
        let changed = render::history_lines_for(&[Message::assistant("gone")]);
        assert!(
            gesture
                .release(&changed, root.path(), area, 0, point)
                .is_none()
        );
        gesture.press(&history, root.path(), area, 0, point);
        std::fs::remove_file(root.path().join("README.md")).unwrap();
        assert!(
            gesture
                .release(&history, root.path(), area, 0, point)
                .is_none()
        );
    }

    #[test]
    fn refuses_urls_missing_files_directories_and_blank_cells() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "synthetic").unwrap();
        for value in [
            "https://example.invalid/README.md",
            "file:///etc/hosts",
            "/missing/file",
            ".",
            "README.md\0",
        ] {
            assert!(local_file(value, root.path()).is_none());
        }
        let history = render::history_lines_for(&[Message::assistant("README.md")]);
        for point in [
            Position::new(70, 0),
            Position::new(0, 1),
            Position::new(0, 9),
        ] {
            assert!(path_at(&history, root.path(), Rect::new(0, 0, 80, 5), 0, point).is_none());
        }
        let user = render::history_lines_for(&[Message::user("README.md")]);
        assert!(
            path_at(
                &user,
                root.path(),
                Rect::new(0, 0, 80, 5),
                0,
                Position::new(0, 0)
            )
            .is_none()
        );
    }

    #[test]
    fn finder_receives_a_literal_reveal_argument_without_a_shell() {
        let root = tempfile::tempdir().unwrap();
        let name = "--literal $(unused); 日本語.txt";
        let path = root.path().join(name);
        std::fs::write(&path, "synthetic").unwrap();
        let path = local_file(name, root.path()).unwrap();
        let command = command(&path);
        assert_eq!(command.get_program(), "/usr/bin/open");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("-R"), path.as_os_str()]
        );
    }
}
