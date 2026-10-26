//! Shared visible-cell selection across TUI screens, with no history or disk sink.
use crate::screen_selection::{Effect, Selection};
use ratatui::{
    Terminal, TerminalOptions,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::{Buffer, Cell},
    crossterm::{
        cursor::{RestorePosition, SavePosition},
        event::{self, Event, MouseEventKind},
        execute, queue,
    },
    layout::{Position, Rect, Size},
    style::{Color, Style},
};
use std::{
    cell::RefCell,
    io::{self, Stdout, Write},
    rc::{Rc, Weak},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

pub(crate) type DefaultTerminal = Terminal<CopyBackend<CrosstermBackend<Stdout>>>;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
thread_local! { static CURRENT: RefCell<Option<(u64, Weak<dyn ScreenAccess>)>> = const { RefCell::new(None) }; }
trait ScreenAccess {
    fn event(&self, event: &Event) -> io::Result<bool>;
    fn clear(&self);
}

pub(crate) trait CursorBackend: Backend {
    type Saved;
    fn save_position(&mut self) -> io::Result<Self::Saved>;
    fn restore_position(&mut self, saved: Self::Saved) -> io::Result<()>;
}
impl<W: Write> CursorBackend for CrosstermBackend<W> {
    type Saved = ();
    fn save_position(&mut self) -> io::Result<()> {
        queue!(self, SavePosition)
    }
    fn restore_position(&mut self, (): ()) -> io::Result<()> {
        queue!(self, RestorePosition)
    }
}

struct Screen<B: CursorBackend> {
    inner: B,
    base: Buffer,
    painted: Buffer,
    selection: Selection,
    notice: Option<(&'static str, Instant)>,
    cursor_visible: bool,
    id: u64,
    owns_mouse: bool,
}
impl<B: CursorBackend> Screen<B> {
    fn reset(&mut self, size: Size) {
        self.base = Buffer::empty(Rect::new(0, 0, size.width, size.height));
        self.painted = self.base.clone();
        self.selection.clear();
        self.notice = None;
    }
    fn sync_size(&mut self) -> io::Result<()> {
        let size = self.inner.size()?;
        if self.base.area.width != size.width || self.base.area.height != size.height {
            self.reset(size);
        }
        Ok(())
    }
    fn compose(&mut self) -> Buffer {
        let mut visible = self.base.clone();
        if self
            .notice
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(2))
        {
            self.notice = None;
        }
        if let Some((text, _)) = self.notice
            && !visible.area.is_empty()
        {
            let width = (text.len() as u16).min(visible.area.width);
            let left = visible.area.right() - width;
            let row = visible.area.bottom() - 1;
            // A notice may start in an occupied wide cell. Clear the whole
            // intersected glyph before writing the displayed overlay.
            let mut x = visible.area.x;
            while x < visible.area.right() {
                let next = x
                    .saturating_add(visible[(x, row)].symbol().width().max(1) as u16)
                    .min(visible.area.right());
                if x < left && next > left {
                    for column in x..next {
                        visible[(column, row)] = Cell::default();
                    }
                }
                x = next;
            }
            for column in left..visible.area.right() {
                visible[(column, row)] = Cell::default();
            }
            visible.set_stringn(
                left,
                row,
                text,
                usize::from(width),
                Style::default().fg(Color::White).bg(Color::DarkGray),
            );
        }
        self.selection.highlight(&mut visible);
        visible
    }
    fn paint(&mut self, preserve_cursor: bool) -> io::Result<()> {
        let visible = self.compose();
        let changes = self.painted.diff(&visible);
        if changes.is_empty() {
            return Ok(());
        }
        let saved = if preserve_cursor {
            Some(self.inner.save_position()?)
        } else {
            None
        };
        if preserve_cursor && self.cursor_visible {
            self.inner.hide_cursor()?;
        }
        let drawn = self.inner.draw(changes.into_iter());
        let restored = if let Some(saved) = saved {
            self.inner.restore_position(saved)
        } else {
            Ok(())
        };
        let shown = if preserve_cursor && self.cursor_visible {
            self.inner.show_cursor()
        } else {
            Ok(())
        };
        drawn?;
        restored?;
        shown?;
        self.painted = visible;
        self.inner.flush()
    }
    fn handle(
        &mut self,
        event: &Event,
        mut copy: impl FnMut(&str) -> Result<(), String>,
    ) -> io::Result<bool> {
        self.sync_size()?;
        if matches!(event, Event::Key(_) | Event::Paste(_)) {
            self.notice = None;
        }
        let visible = self.compose();
        let consumed = match self.selection.event(event, &visible) {
            Effect::Pass => false,
            Effect::Paint => true,
            Effect::Copy(text) => {
                let message = if copy(&text).is_ok() {
                    "Copied"
                } else {
                    "Copy failed"
                };
                self.notice = Some((message, Instant::now()));
                true
            }
        };
        if matches!(event, Event::Resize(_, _)) {
            // The next Ratatui draw performs its own autoresize/clear. Do not
            // retain a former viewport as a copy source in the meantime.
            let size = self.inner.size()?;
            self.reset(size);
        } else {
            self.paint(true)?;
        }
        Ok(consumed)
    }
}
impl<B: CursorBackend> Drop for Screen<B> {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            let is_current = slot.borrow().as_ref().is_some_and(|(id, _)| *id == self.id);
            if is_current {
                *slot.borrow_mut() = None;
                if self.owns_mouse {
                    let _ = execute!(io::stdout(), event::DisableMouseCapture);
                }
            }
        });
    }
}
impl<B: CursorBackend + 'static> ScreenAccess for RefCell<Screen<B>> {
    fn event(&self, event: &Event) -> io::Result<bool> {
        self.try_borrow_mut()
            .map_err(|_| io::Error::other("screen is being drawn"))?
            .handle(event, crate::clipboard::copy_to_clipboard)
    }
    fn clear(&self) {
        if let Ok(mut screen) = self.try_borrow_mut() {
            screen.base = Buffer::empty(Rect::default());
            screen.painted = screen.base.clone();
            screen.selection.clear();
            screen.notice = None;
        }
    }
}

pub(crate) struct CopyBackend<B: CursorBackend> {
    screen: Rc<RefCell<Screen<B>>>,
}
impl<B: CursorBackend + 'static> CopyBackend<B> {
    fn new(inner: B, register: bool) -> Self {
        let screen = Rc::new(RefCell::new(Screen {
            inner,
            base: Buffer::empty(Rect::default()),
            painted: Buffer::empty(Rect::default()),
            selection: Selection::default(),
            notice: None,
            cursor_visible: true,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            owns_mouse: false,
        }));
        let backend = Self { screen };
        if register {
            backend.activate(true);
        }
        backend
    }
    fn activate(&self, mouse: bool) {
        let screen = &self.screen;
        let old = CURRENT.with(|slot| slot.borrow_mut().take());
        if let Some((_, old)) = old
            && let Some(old) = old.upgrade()
        {
            old.clear();
        }
        let erased: Rc<dyn ScreenAccess> = screen.clone();
        CURRENT
            .with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&erased))));
        if mouse {
            screen.borrow_mut().owns_mouse =
                execute!(io::stdout(), event::EnableMouseCapture).is_ok();
        }
    }
}
impl<B: CursorBackend + 'static> Backend for CopyBackend<B> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut screen = self.screen.borrow_mut();
        screen.sync_size()?;
        let mut changed_text = false;
        for (x, y, cell) in content {
            if !screen.base.area.contains((x, y).into()) {
                continue;
            }
            changed_text |= screen.base[(x, y)].symbol() != cell.symbol();
            screen.base[(x, y)] = cell.clone();
            // Ratatui omits occupied follower cells from its diff iterator.
            let width = cell.symbol().width().max(1).min(usize::from(u16::MAX)) as u16;
            for column in x.saturating_add(1)..x.saturating_add(width).min(screen.base.area.right())
            {
                screen.base[(column, y)] = Cell::default();
            }
        }
        if changed_text && !screen.selection.dragging() {
            screen.selection.clear();
        }
        screen.paint(false)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        let mut s = self.screen.borrow_mut();
        s.inner.hide_cursor()?;
        s.cursor_visible = false;
        Ok(())
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        let mut s = self.screen.borrow_mut();
        s.inner.show_cursor()?;
        s.cursor_visible = true;
        Ok(())
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.screen.borrow_mut().inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
        self.screen.borrow_mut().inner.set_cursor_position(p)
    }
    fn clear(&mut self) -> io::Result<()> {
        let mut s = self.screen.borrow_mut();
        s.inner.clear()?;
        let size = s.inner.size()?;
        s.reset(size);
        Ok(())
    }
    fn clear_region(&mut self, kind: ClearType) -> io::Result<()> {
        if kind == ClearType::All {
            return self.clear();
        }
        let mut s = self.screen.borrow_mut();
        s.sync_size()?;
        let cursor = s.inner.get_cursor_position()?;
        s.inner.clear_region(kind)?;
        for y in s.base.area.y..s.base.area.bottom() {
            for x in s.base.area.x..s.base.area.right() {
                let clear = match kind {
                    ClearType::AfterCursor => (y, x) >= (cursor.y, cursor.x),
                    ClearType::BeforeCursor => (y, x) <= (cursor.y, cursor.x),
                    ClearType::CurrentLine => y == cursor.y,
                    ClearType::UntilNewLine => y == cursor.y && x >= cursor.x,
                    ClearType::All => true,
                };
                if clear {
                    s.base[(x, y)] = Cell::default();
                    s.painted[(x, y)] = Cell::default();
                }
            }
        }
        s.selection.clear();
        s.notice = None;
        s.paint(true)
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        let mut s = self.screen.borrow_mut();
        s.sync_size()?;
        let cursor = s.inner.get_cursor_position()?;
        s.inner.append_lines(n)?;
        let overflow = cursor
            .y
            .saturating_add(n)
            .saturating_add(1)
            .saturating_sub(s.base.area.height);
        let height = s.base.area.height;
        shift(&mut s.base, 0..height, overflow, true);
        shift(&mut s.painted, 0..height, overflow, true);
        s.selection.clear();
        s.notice = None;
        s.paint(false)
    }
    fn size(&self) -> io::Result<Size> {
        self.screen.borrow().inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.screen.borrow_mut().inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        self.screen.borrow_mut().inner.flush()
    }
    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, n: u16) -> io::Result<()> {
        let mut s = self.screen.borrow_mut();
        s.sync_size()?;
        s.inner.scroll_region_up(region.clone(), n)?;
        shift(&mut s.base, region.clone(), n, true);
        shift(&mut s.painted, region, n, true);
        s.selection.clear();
        s.notice = None;
        s.paint(false)
    }
    fn scroll_region_down(&mut self, region: std::ops::Range<u16>, n: u16) -> io::Result<()> {
        let mut s = self.screen.borrow_mut();
        s.sync_size()?;
        s.inner.scroll_region_down(region.clone(), n)?;
        shift(&mut s.base, region.clone(), n, false);
        shift(&mut s.painted, region, n, false);
        s.selection.clear();
        s.notice = None;
        s.paint(false)
    }
}
fn shift(buffer: &mut Buffer, region: std::ops::Range<u16>, n: u16, up: bool) {
    let old = buffer.clone();
    for y in region.start..region.end.min(buffer.area.bottom()) {
        for x in buffer.area.x..buffer.area.right() {
            let source = if up {
                y.checked_add(n).filter(|row| *row < region.end)
            } else {
                y.checked_sub(n).filter(|row| *row >= region.start)
            };
            buffer[(x, y)] = source.map(|row| old[(x, row)].clone()).unwrap_or_default();
        }
    }
}

pub(crate) fn backend() -> CopyBackend<CrosstermBackend<Stdout>> {
    CopyBackend::new(CrosstermBackend::new(io::stdout()), true)
}
pub(crate) fn filter_event(event: Event) -> io::Result<Event> {
    let active = CURRENT.with(|slot| slot.borrow().as_ref().and_then(|(_, weak)| weak.upgrade()));
    if let Some(active) = active
        && active.event(&event)?
    {
        Ok(Event::FocusGained)
    } else {
        Ok(event)
    }
}
pub(crate) fn read_event() -> io::Result<Event> {
    // Drain selection/motion packets already queued by the terminal. Callback
    // and worker waits poll at finite intervals; one packet per interval would
    // leave mouse-up (and its copy) behind a long motion backlog.
    for _ in 0..64 {
        let event = filter_event(event::read()?)?;
        let transient = matches!(
            event,
            Event::FocusGained
                | Event::Mouse(event::MouseEvent {
                    kind: MouseEventKind::Moved,
                    ..
                })
        );
        if !transient || !event::poll(Duration::ZERO)? {
            return Ok(event);
        }
    }
    Ok(Event::FocusGained)
}
pub(crate) fn restore() {
    let active = CURRENT.with(|slot| slot.borrow_mut().take());
    if let Some((_, active)) = active
        && let Some(active) = active.upgrade()
    {
        active.clear();
    }
    let _ = execute!(io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
}
fn panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}
pub(crate) fn init_with_options(options: TerminalOptions) -> DefaultTerminal {
    panic_hook();
    ratatui::crossterm::terminal::enable_raw_mode().expect("failed to initialize terminal");
    Terminal::with_options(backend(), options).expect("failed to initialize terminal")
}
pub(crate) fn init() -> DefaultTerminal {
    panic_hook();
    ratatui::crossterm::terminal::enable_raw_mode().expect("failed to initialize terminal");
    execute!(
        io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    )
    .expect("failed to enter alternate screen");
    Terminal::new(backend()).expect("failed to initialize terminal")
}

#[cfg(test)]
impl CursorBackend for ratatui::backend::TestBackend {
    type Saved = Position;
    fn save_position(&mut self) -> io::Result<Position> {
        self.get_cursor_position()
    }
    fn restore_position(&mut self, saved: Position) -> io::Result<()> {
        self.set_cursor_position(saved)
    }
}
#[cfg(test)]
pub(crate) fn test_terminal(
    width: u16,
    height: u16,
) -> Terminal<CopyBackend<ratatui::backend::TestBackend>> {
    Terminal::new(CopyBackend::new(
        ratatui::backend::TestBackend::new(width, height),
        false,
    ))
    .unwrap()
}
#[cfg(test)]
pub(crate) fn test_copy_all(
    terminal: &mut Terminal<CopyBackend<ratatui::backend::TestBackend>>,
) -> String {
    use event::{KeyModifiers, MouseButton, MouseEvent};
    let mut screen = terminal.backend_mut().screen.borrow_mut();
    let area = screen.base.area;
    let down = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    });
    let up = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: area.right(),
        row: area.bottom() - 1,
        modifiers: KeyModifiers::NONE,
    });
    let mut result = String::new();
    screen
        .handle(&down, |_| panic!("press must not copy"))
        .unwrap();
    screen
        .handle(&up, |text| {
            result = text.to_owned();
            Ok(())
        })
        .unwrap();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
    use ratatui::{backend::TestBackend, widgets::Paragraph};
    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }
    #[test]
    fn replacing_a_terminal_clears_old_copy_source_and_old_drop_keeps_new_owner() {
        let mut old = test_terminal(30, 4);
        old.backend().activate(false);
        old.draw(|f| f.render_widget(Paragraph::new("old-ephemeral-url"), f.area()))
            .unwrap();
        let mut new = test_terminal(30, 4);
        new.backend().activate(false);
        assert!(old.backend().screen.borrow().base.area.is_empty());
        new.draw(|f| f.render_widget(Paragraph::new("new visible page"), f.area()))
            .unwrap();
        let id = new.backend().screen.borrow().id;
        drop(old);
        assert!(CURRENT.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|(current, _)| *current == id)
        }));
        assert!(test_copy_all(&mut new).contains("new visible page"));
        assert!(!test_copy_all(&mut new).contains("old-ephemeral"));
        drop(new);
        assert!(CURRENT.with(|slot| slot.borrow().is_none()));
    }
    #[test]
    fn copy_failure_is_static_and_never_aborts_the_current_ui_owner() {
        let mut terminal = test_terminal(30, 4);
        terminal
            .draw(|f| f.render_widget(Paragraph::new("visible synthetic"), f.area()))
            .unwrap();
        let mut screen = terminal.backend_mut().screen.borrow_mut();
        screen
            .handle(
                &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
                |_| panic!(),
            )
            .unwrap();
        assert!(
            screen
                .handle(
                    &mouse(MouseEventKind::Up(MouseButton::Left), 7, 0),
                    |text| {
                        assert_eq!(text, "visible");
                        Err("do not echo user clipboard data".into())
                    }
                )
                .unwrap()
        );
        assert_eq!(
            screen.notice.map(|(message, _)| message),
            Some("Copy failed")
        );
    }

    #[test]
    fn displayed_copy_notice_can_itself_be_selected_without_copying_underlying_text() {
        let mut terminal = test_terminal(30, 4);
        terminal
            .draw(|f| f.render_widget(Paragraph::new("body"), f.area()))
            .unwrap();
        let mut screen = terminal.backend_mut().screen.borrow_mut();
        screen.notice = Some(("Copy failed", Instant::now()));
        screen.paint(true).unwrap();
        let mut copied = String::new();
        screen
            .handle(
                &mouse(MouseEventKind::Down(MouseButton::Left), 19, 3),
                |_| panic!(),
            )
            .unwrap();
        screen
            .handle(
                &mouse(MouseEventKind::Up(MouseButton::Left), 30, 3),
                |text| {
                    copied = text.into();
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(copied, "Copy failed");
    }

    #[test]
    fn wide_transitions_and_overlay_restore_keep_base_physical_screen_and_copy_consistent() {
        let mut terminal = test_terminal(16, 4);
        terminal
            .draw(|f| f.render_widget(Paragraph::new("aa"), f.area()))
            .unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new("日"), f.area()))
            .unwrap();
        assert_eq!(test_copy_all(&mut terminal).trim(), "日");
        terminal.backend_mut().screen.borrow_mut().notice = None;
        terminal
            .draw(|f| f.render_widget(Paragraph::new("abc"), f.area()))
            .unwrap();
        assert_eq!(test_copy_all(&mut terminal).trim(), "abc");
        let mut s = terminal.backend_mut().screen.borrow_mut();
        s.notice = None;
        s.selection.clear();
        s.paint(true).unwrap();
        s.inner.set_cursor_position(Position::new(8, 2)).unwrap();
        s.handle(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            |_| panic!(),
        )
        .unwrap();
        s.handle(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            |_| panic!(),
        )
        .unwrap();
        assert!(
            s.inner.buffer()[(2, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        s.handle(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 1, 0),
            |_| panic!(),
        )
        .unwrap();
        assert!(
            !s.inner.buffer()[(2, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(s.inner.get_cursor_position().unwrap(), Position::new(8, 2));
        drop(s);
        terminal
            .draw(|f| f.render_widget(Paragraph::new("XYZ"), f.area()))
            .unwrap();
        let mut s = terminal.backend_mut().screen.borrow_mut();
        let mut copied = String::new();
        s.handle(
            &mouse(MouseEventKind::Up(MouseButton::Left), 2, 0),
            |text| {
                copied = text.into();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(copied, "XY");
        s.handle(
            &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            |_| panic!(),
        )
        .unwrap();
        assert_eq!(s.inner.buffer(), &s.base);
    }
    #[test]
    fn resize_clear_and_new_page_do_not_copy_old_text() {
        let mut terminal = test_terminal(20, 4);
        terminal
            .draw(|f| f.render_widget(Paragraph::new("old-private-url"), f.area()))
            .unwrap();
        assert!(test_copy_all(&mut terminal).contains("old-private-url"));
        terminal
            .backend_mut()
            .screen
            .borrow_mut()
            .inner
            .resize(10, 4);
        terminal.resize(Rect::new(0, 0, 10, 4)).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new("new"), f.area()))
            .unwrap();
        let text = test_copy_all(&mut terminal);
        assert!(text.contains("new"));
        assert!(!text.contains("old-private"));
        terminal.clear().unwrap();
        assert!(test_copy_all(&mut terminal).trim().is_empty());
    }
    #[test]
    fn actual_onboarding_picker_approval_and_main_frames_are_copyable() {
        type Case = (
            &'static str,
            Box<dyn FnMut(&mut ratatui::Frame)>,
            &'static str,
        );
        let cases: Vec<Case> = vec![
            (
                "onboarding",
                Box::new(|f| {
                    crate::onboarding::render_choice_screen(f, crate::onboarding::Choice::ChatGpt)
                }),
                "Sign in with ChatGPT",
            ),
            (
                "registered-onboarding",
                Box::new(crate::onboarding::render_registered_wait),
                "Authorize in your browser",
            ),
            (
                "api-key",
                Box::new(|f| crate::onboarding::render_api_key_prompt(f, 12)),
                "************",
            ),
            (
                "model",
                Box::new(|f| crate::render::render_model_picker(f, "gpt-6.1-sol", false, 0)),
                "gpt-6.1-sol",
            ),
            (
                "resume",
                Box::new(|f| crate::render::render_resume_picker(f, &[], 0, 0)),
                "no saved conversations",
            ),
            (
                "approval",
                Box::new(|f| crate::render::render_approval_modal(f, "synthetic operation 日本語")),
                "synthetic operation 日本語",
            ),
            (
                "management",
                Box::new(|f| {
                    crate::render::render_management_confirmation(
                        f,
                        "operation",
                        "target",
                        "synthetic detail",
                    );
                }),
                "synthetic detail",
            ),
            (
                "main",
                Box::new(|f| {
                    let history = vec![crate::render::HistoryLine {
                        line: ratatui::text::Line::from("history 日本語 /help /status"),
                        shaded: false,
                    }];
                    let mut cache = crate::render::HistoryWrapCache::default();
                    crate::draw_frame(
                        f,
                        &history,
                        &mut cache,
                        0,
                        "input 日本語",
                        0,
                        &crate::Status::Notice("working status".into()),
                        &crate::render::HeaderInfo {
                            provider_name: "fixture",
                            model_name: "fixture-model",
                            usage: Default::default(),
                            cwd: "/synthetic",
                            cwd_short: "/synthetic",
                            effort_name: "low",
                        },
                        &[],
                        0,
                        &None,
                    );
                }),
                "input 日本語",
            ),
        ];
        for (name, mut draw, marker) in cases {
            let mut terminal = test_terminal(120, 30);
            terminal.draw(|f| draw(f)).unwrap();
            let copied = test_copy_all(&mut terminal);
            assert!(copied.contains(marker), "{name}: {copied}");
            assert!(!copied.contains("not-rendered-secret-canary"));
        }
        let mut terminal = test_terminal(120, 30);
        let url = "https://auth.openai.com/oauth/authorize?state=synthetic&code_challenge=dummy";
        crate::auth_browser::draw_link(&mut terminal, url, "Waiting", 0).unwrap();
        assert!(test_copy_all(&mut terminal).contains(url));
        crate::auth_browser::draw_link(&mut terminal, "", "Finishing sign-in...", 0).unwrap();
        assert!(!test_copy_all(&mut terminal).contains("code_challenge"));
    }
    struct TestAccess {
        screen: Rc<RefCell<Screen<TestBackend>>>,
        copied: Rc<RefCell<Vec<String>>>,
    }
    impl ScreenAccess for TestAccess {
        fn event(&self, event: &Event) -> io::Result<bool> {
            self.screen.borrow_mut().handle(event, |text| {
                self.copied.borrow_mut().push(text.into());
                Ok(())
            })
        }
        fn clear(&self) {
            let size = self.screen.borrow().inner.size().unwrap();
            self.screen.borrow_mut().reset(size);
        }
    }
    #[tokio::test]
    async fn idle_and_working_stream_reducers_copy_immediately_without_a_management_owner() {
        use futures_util::StreamExt;
        let mut terminal = test_terminal(24, 4);
        terminal
            .draw(|f| f.render_widget(Paragraph::new("visible 日本語"), f.area()))
            .unwrap();
        let screen = terminal.backend().screen.clone();
        let copied = Rc::new(RefCell::new(Vec::new()));
        let access: Rc<dyn ScreenAccess> = Rc::new(TestAccess {
            screen: screen.clone(),
            copied: copied.clone(),
        });
        CURRENT
            .with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&access))));
        let events = vec![
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            mouse(MouseEventKind::Drag(MouseButton::Left), 7, 0),
            mouse(MouseEventKind::Up(MouseButton::Left), 7, 0),
        ];
        for event in events.clone() {
            assert!(matches!(filter_event(event).unwrap(), Event::FocusGained));
        }
        assert_eq!(copied.borrow().as_slice(), ["visible"]);
        let mut stream = futures_util::stream::iter(events.into_iter().map(Ok::<_, io::Error>));
        while let Some(event) = stream.next().await {
            assert!(matches!(
                event.and_then(filter_event).unwrap(),
                Event::FocusGained
            ));
        }
        assert_eq!(copied.borrow().as_slice(), ["visible", "visible"]);
        assert!(matches!(
            filter_event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            )))
            .unwrap(),
            Event::Key(_)
        ));
        let old = screen.borrow().id;
        CURRENT.with(|slot| *slot.borrow_mut() = None);
        drop(access);
        drop(terminal);
        assert!(
            CURRENT.with(|slot| slot.borrow().is_none()),
            "old terminal {old} retained"
        );
    }
    #[test]
    fn shared_filter_routes_only_modified_complete_clicks_to_auth_and_finder() {
        let mut terminal = test_terminal(120, 20);
        let copied = Rc::new(RefCell::new(Vec::new()));
        let screen = terminal.backend().screen.clone();
        let access: Rc<dyn ScreenAccess> = Rc::new(TestAccess {
            screen: screen.clone(),
            copied: copied.clone(),
        });
        CURRENT
            .with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&access))));
        let url = "https://example.invalid/authorize?state=synthetic";
        let rect = crate::auth_browser::draw_link(&mut terminal, url, "Waiting", 0).unwrap();
        for modifiers in [
            KeyModifiers::NONE,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            for drag in [false, true] {
                let mut click = crate::screen_selection::LinkClick::default();
                let mut count = 0;
                let mut event = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x,
                    row: rect.y,
                    modifiers,
                };
                count += usize::from(crate::auth_browser::test_link_event(
                    filter_event(Event::Mouse(event)).unwrap(),
                    rect,
                    url,
                    &mut click,
                ));
                assert_eq!(count, 0, "press must not launch");
                if drag {
                    event.kind = MouseEventKind::Drag(MouseButton::Left);
                    count += usize::from(crate::auth_browser::test_link_event(
                        filter_event(Event::Mouse(event)).unwrap(),
                        rect,
                        url,
                        &mut click,
                    ));
                }
                event.kind = MouseEventKind::Up(MouseButton::Left);
                count += usize::from(crate::auth_browser::test_link_event(
                    filter_event(Event::Mouse(event)).unwrap(),
                    rect,
                    url,
                    &mut click,
                ));
                assert_eq!(
                    count,
                    usize::from(crate::screen_selection::link_modifier(modifiers) && !drag)
                );
                assert!(!crate::auth_browser::test_link_event(
                    filter_event(Event::Mouse(event)).unwrap(),
                    rect,
                    url,
                    &mut click
                ));
            }
        }
        let mut blank_gesture = crate::screen_selection::LinkClick::default();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let event = MouseEvent {
                kind,
                column: rect.right() - 1,
                row: rect.bottom() - 1,
                modifiers: KeyModifiers::CONTROL,
            };
            assert!(!crate::auth_browser::test_link_event(
                filter_event(Event::Mouse(event)).unwrap(),
                rect,
                url,
                &mut blank_gesture
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("target.txt"), "synthetic").unwrap();
            let history = vec![crate::render::HistoryLine {
                line: ratatui::text::Line::from("target.txt"),
                shaded: false,
            }];
            terminal
                .draw(|f| f.render_widget(Paragraph::new("target.txt"), f.area()))
                .unwrap();
            let area = Rect::new(0, 0, 120, 20);
            for modifiers in [
                KeyModifiers::NONE,
                KeyModifiers::CONTROL,
                KeyModifiers::SUPER,
            ] {
                for drag in [false, true] {
                    let mut click = crate::file_reveal::FileClick::default();
                    let mut count = 0;
                    let mut event = MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: 1,
                        row: 0,
                        modifiers,
                    };
                    let down = filter_event(Event::Mouse(event)).unwrap();
                    if let Event::Mouse(mouse) = down {
                        assert!(click.mouse(mouse, &history, root.path(), area, 0).is_none());
                    }
                    if drag {
                        event.kind = MouseEventKind::Drag(MouseButton::Left);
                        if let Event::Mouse(mouse) = filter_event(Event::Mouse(event)).unwrap() {
                            assert!(click.mouse(mouse, &history, root.path(), area, 0).is_none());
                        }
                    }
                    event.kind = MouseEventKind::Up(MouseButton::Left);
                    if let Event::Mouse(mouse) = filter_event(Event::Mouse(event)).unwrap() {
                        count += usize::from(
                            click.mouse(mouse, &history, root.path(), area, 0).is_some(),
                        );
                    }
                    assert_eq!(
                        count,
                        usize::from(crate::screen_selection::link_modifier(modifiers) && !drag)
                    );
                }
            }
        }
        assert!(
            copied.borrow().is_empty(),
            "link gestures must not cause a clipboard write"
        );
        CURRENT.with(|slot| *slot.borrow_mut() = None);
    }

    #[test]
    fn cursor_protocol_uses_save_and_restore_without_a_cursor_query() {
        let mut bytes = Vec::new();
        {
            let mut backend = CrosstermBackend::new(&mut bytes);
            backend.save_position().unwrap();
            backend.restore_position(()).unwrap();
        }
        assert_eq!(bytes.as_slice(), b"\x1b7\x1b8");
    }
    #[test]
    fn production_runtime_factories_and_sync_async_readers_use_the_common_path() {
        for (name, source) in [
            ("main", include_str!("lib.rs")),
            ("onboarding", include_str!("onboarding.rs")),
            ("approver", include_str!("approver.rs")),
            ("account", include_str!("account.rs")),
            ("env", include_str!("environment_commands.rs")),
            ("auth", include_str!("auth_browser.rs")),
            ("windows", include_str!("windows_read_only.rs")),
        ] {
            assert!(
                !source.contains("ratatui::crossterm::event::read("),
                "raw reader: {name}"
            );
            assert!(
                !source.contains("event::read()"),
                "raw alias reader: {name}"
            );
            assert!(!source.contains("ratatui::init("), "unwrapped init: {name}");
            assert!(
                !source.contains("ratatui::init_with_options("),
                "unwrapped init options: {name}"
            );
        }
        assert!(include_str!("lib.rs").contains("event.and_then(crate::terminal::filter_event)"));
    }
}

#[cfg(test)]
mod native_fixture {
    use super::*;
    use ratatui::widgets::Paragraph;
    #[test]
    #[ignore = "synthetic OS-PTY fixture entered only by the scoped screen-copy runner"]
    fn native_screen_copy_driver() {
        let root = std::path::PathBuf::from(
            std::env::var_os("POLARIS_TEST_COPY_DRIVER_PATH")
                .expect("scoped PTY fixture path required"),
        );
        assert!(
            root.canonicalize().unwrap().starts_with(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/verification-fixtures")
                    .canonicalize()
                    .unwrap()
            )
        );
        assert!(
            !std::fs::symlink_metadata(&root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        struct Capture {
            screen: Rc<RefCell<Screen<CrosstermBackend<Stdout>>>>,
            samples: RefCell<Vec<String>>,
            root: std::path::PathBuf,
        }
        impl ScreenAccess for Capture {
            fn event(&self, event: &Event) -> io::Result<bool> {
                self.screen.borrow_mut().handle(event, |text| {
                    self.samples.borrow_mut().push(text.into());
                    std::fs::write(
                        self.root.join("clipboard.json"),
                        serde_json::to_vec(&*self.samples.borrow()).unwrap(),
                    )
                    .map_err(|_| "fixture capture failed".into())
                })
            }
            fn clear(&self) {
                let mut screen = self.screen.borrow_mut();
                screen.base = Buffer::empty(Rect::default());
                screen.painted = screen.base.clone();
                screen.selection.clear();
            }
        }
        let mut terminal = init();
        let screen = terminal.backend().screen.clone();
        let capture: Rc<dyn ScreenAccess> = Rc::new(Capture {
            screen: screen.clone(),
            samples: RefCell::new(Vec::new()),
            root: root.clone(),
        });
        CURRENT
            .with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&capture))));
        let working = std::env::var_os("POLARIS_TEST_COPY_DRIVER_WORKING").is_some();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use futures_util::StreamExt;
            let mut stream=working.then(event::EventStream::new);
            let url="https://example.invalid/?synthetic=1";
            let rect=Rect::new(0,2,80,1);
            let mut link=crate::screen_selection::LinkClick::default();
            let mut opened=0usize;let mut barriers=0usize;let mut page=false;
            terminal.draw(|f|f.render_widget(Paragraph::new(format!("COPY_READY\nA日e\u{301}B\n{url}\nlast line")),f.area())).unwrap();
            std::fs::write(root.join("metrics.json"),serde_json::to_vec(&serde_json::json!({"ready":true,"opened":opened,"barriers":barriers})).unwrap()).unwrap();
            loop {
                let event=if let Some(stream)=&mut stream {stream.next().await.unwrap().and_then(filter_event).unwrap()} else {read_event().unwrap()};
                if crate::auth_browser::test_link_event(event.clone(),rect,url,&mut link) {opened+=1;}
                if let Event::Key(key)=event {
                    if key.code==event::KeyCode::Char('q') {break;}
                    if key.code==event::KeyCode::Char('b') {barriers+=1;}
                    if key.code==event::KeyCode::Char('p') {
                        page=true;
                        terminal.draw(|f|f.render_widget(Paragraph::new("NEW_PAGE\nsecond\n"),f.area())).unwrap();
                    }
                }
                std::fs::write(root.join("metrics.json"),serde_json::to_vec(&serde_json::json!({"ready":true,"opened":opened,"barriers":barriers,"page":page})).unwrap()).unwrap();
            }
        });
        restore();
    }
}
