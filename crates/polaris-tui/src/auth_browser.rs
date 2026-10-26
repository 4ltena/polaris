//! Ephemeral OAuth UI. URLs stay on this screen, never in conversation output.
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
        execute,
    },
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{io, time::Duration};

struct MouseCapture(bool);
impl Drop for MouseCapture {
    fn drop(&mut self) {
        if self.0 {
            let _ = execute!(io::stdout(), event::DisableMouseCapture);
        }
    }
}

fn render(frame: &mut Frame, url: &str, status: &str, scroll: u16) -> Rect {
    if url.is_empty() {
        frame.render_widget(
            Paragraph::new(status.to_owned()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("polaris login"),
            ),
            frame.area(),
        );
        return Rect::default();
    }
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title("polaris login");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let heading = Rect::new(inner.x, inner.y, inner.width, inner.height.min(5));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Authorize in your browser"),
            Line::from("Enter/o or click URL: open | c: copy link | Esc/Ctrl-C: cancel"),
            Line::from("Up/Down: scroll link"),
            Line::from(status.to_owned()),
            Line::from("Authorization link (includes query):"),
        ])
        .wrap(Wrap { trim: false }),
        heading,
    );
    let link = Rect::new(
        inner.x,
        inner.y.saturating_add(5),
        inner.width,
        inner.height.saturating_sub(5),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            url.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        )))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0)),
        link,
    );
    link
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Open,
    Copy,
    Cancel,
    Up,
    Down,
    None,
}
fn action(event: Event, link: Rect, url: &str, scroll: u16) -> Action {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Esc => Action::Cancel,
            KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Action::Cancel
            }
            KeyCode::Char('c') => Action::Copy,
            KeyCode::Char('o') | KeyCode::Enter => Action::Open,
            KeyCode::Up => Action::Up,
            KeyCode::Down => Action::Down,
            _ => Action::None,
        },
        Event::Mouse(mouse)
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && link.contains((mouse.column, mouse.row).into())
                // authorize_url produces an ASCII, percent-encoded URL. Its
                // wrapped cells map directly to byte offsets, including scroll.
                && ((usize::from(mouse.row - link.y) + usize::from(scroll))
                    * usize::from(link.width) + usize::from(mouse.column - link.x)) < url.len() =>
        {
            Action::Open
        }
        _ => Action::None,
    }
}

fn copy_link(url: &str, copy: impl FnOnce(&str) -> Result<(), String>) -> &'static str {
    if copy(url).is_ok() {
        "Authorization link copied. Keep this sign-in screen open."
    } else {
        "Could not copy link. Open the displayed URL manually."
    }
}

pub(crate) fn draw_link<B: Backend>(
    terminal: &mut Terminal<B>,
    url: &str,
    status: &str,
    scroll: u16,
) -> Result<Rect, polaris_auth::AuthError> {
    let mut link = Rect::default();
    terminal
        .draw(|frame| link = render(frame, url, status, scroll))
        .map_err(|_| polaris_auth::AuthError::Denied("Cannot display sign-in link".into()))?;
    Ok(link)
}

pub(crate) async fn callback(
    url: String,
    state: String,
    own_mouse: bool,
    mut draw: impl FnMut(&str, &str, u16) -> Result<Rect, polaris_auth::AuthError>,
) -> Result<String, polaris_auth::AuthError> {
    use polaris_auth::{AuthError, login};
    // Bind first: a manually opened link must never race listener setup.
    let listener = login::bind_callback().await?;
    let mut mouse = MouseCapture(false);
    if own_mouse {
        execute!(io::stdout(), event::EnableMouseCapture)?;
        mouse.0 = true;
    }
    let callback = login::wait_for_callback(listener, &state, Duration::from_secs(300));
    tokio::pin!(callback);
    let mut launch = Box::pin(login::launch_browser(&url));
    let mut launch_done = false;
    let mut status = "Requesting your default browser...";
    let mut scroll = 0u16;
    loop {
        let link = draw(&url, status, scroll)?;
        tokio::select! {
            result = &mut callback => {
                // Remove the ephemeral link before token exchange starts.
                draw("", "Finishing sign-in...", 0)?;
                return result;
            },
            result = &mut launch, if !launch_done => {
                launch_done = true;
                status = if result.is_ok() { "Browser requested. You can also open or copy the link below." }
                    else { "Could not open browser. Open or copy the link below; sign-in is still waiting." };
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                #[cfg(target_os = "linux")]
                if crate::signal::pending_linux_process_shutdown().is_some() {
                    return Err(AuthError::Denied("cancelled".into()));
                }
                if !event::poll(Duration::ZERO)? { continue; }
                match action(event::read()?, link, &url, scroll) {
                    Action::Cancel => return Err(AuthError::Denied("cancelled".into())),
                    Action::Copy => {
                        status = copy_link(&url, crate::clipboard::copy_to_clipboard);
                    }
                    Action::Open if launch_done => {
                        launch = Box::pin(login::launch_browser(&url));
                        launch_done = false;
                        status = "Requesting your default browser...";
                    }
                    Action::Up => scroll = scroll.saturating_sub(1),
                    Action::Down => scroll = scroll.saturating_add(1).min(url.len().min(u16::MAX as usize) as u16),
                    _ => {}
                }
            }
        }
    }
}

/// The callback UI owns input until it receives a code. Existing cancellation
/// then remains active throughout token exchange and storage.
pub(crate) async fn cancel_after_callback(ready: tokio::sync::oneshot::Receiver<()>) {
    let _ = ready.await;
    crate::account::browser_cancel().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::TestBackend,
        crossterm::event::{KeyEvent, MouseEvent},
    };

    #[test]
    fn only_visible_url_cells_are_underlined_and_clickable_after_wrap_scroll_and_resize() {
        let url = "https://example.invalid/authorize?state=synthetic-only";
        for (width, scroll) in [(24, 0), (24, 1), (80, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 15)).unwrap();
            let rect = draw_link(&mut terminal, url, "Waiting", scroll).unwrap();
            let mut visible = 0;
            for row in rect.y..rect.bottom() {
                for column in rect.x..rect.right() {
                    let index = (usize::from(row - rect.y) + usize::from(scroll))
                        * usize::from(rect.width)
                        + usize::from(column - rect.x);
                    let is_url = index < url.len();
                    let cell = &terminal.backend().buffer()[(column, row)];
                    assert_eq!(
                        cell.modifier.contains(Modifier::UNDERLINED),
                        is_url,
                        "underline at {column},{row}, width={width}, scroll={scroll}"
                    );
                    let click = Event::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    });
                    assert_eq!(
                        action(click, rect, url, scroll),
                        if is_url { Action::Open } else { Action::None }
                    );
                    if is_url {
                        visible += 1;
                    }
                }
            }
            assert!(visible > 0);
        }
    }

    #[test]
    fn completing_authorization_clears_link_and_stale_controls_from_screen() {
        let mut terminal = Terminal::new(TestBackend::new(140, 12)).unwrap();
        draw_link(
            &mut terminal,
            "https://auth.openai.com/oauth/authorize?state=synthetic-private",
            "Waiting",
            0,
        )
        .unwrap();
        draw_link(&mut terminal, "", "Finishing sign-in...", 0).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Finishing sign-in"));
        assert!(!text.contains("synthetic-private"));
        assert!(!text.contains("copy link"));
    }

    #[test]
    fn copy_passes_the_entire_query_to_the_clipboard_without_echoing_it_in_status() {
        let url =
            "https://auth.openai.com/oauth/authorize?state=synthetic&code_challenge=long-dummy";
        let mut copied = String::new();
        let status = copy_link(url, |text| {
            copied = text.to_owned();
            Ok(())
        });
        assert_eq!(copied, url);
        assert!(status.contains("copied"));
        assert!(!status.contains("synthetic"));
        assert!(copy_link(url, |_| Err("dummy error".into())).contains("Could not copy"));
    }

    #[test]
    fn link_including_query_is_visible_and_actions_target_it() {
        let url = "https://auth.openai.com/oauth/authorize?client_id=synthetic&state=dummy&code_challenge=synthetic";
        let mut terminal = Terminal::new(TestBackend::new(140, 12)).unwrap();
        let mut rect = Rect::default();
        terminal
            .draw(|f| rect = render(f, url, "Could not open browser", 0))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains(url));
        assert!(text.contains("Could not open browser"));
        assert_eq!(
            action(
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
                rect,
                url,
                0
            ),
            Action::Copy
        );
        assert_eq!(
            action(
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                rect,
                url,
                0
            ),
            Action::Cancel
        );
        assert_eq!(
            action(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE
                }),
                rect,
                url,
                0
            ),
            Action::Open
        );
        assert_eq!(
            action(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE
                }),
                rect,
                url,
                0
            ),
            Action::None
        );
        terminal.backend_mut().resize(40, 12);
        terminal.resize(Rect::new(0, 0, 40, 12)).unwrap();
        terminal
            .draw(|f| {
                render(f, url, "Copy full link with c", 1);
            })
            .unwrap();
    }
}
