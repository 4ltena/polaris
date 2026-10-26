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
#[path = "terminal/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "terminal/native_fixture.rs"]
mod native_fixture;
