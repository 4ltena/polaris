//! The onboarding screen: shown by `polaris-cli` when the interactive TUI
//! starts with no usable OpenAI credential. Lets the user sign in with
//! ChatGPT or enter an API key without restarting the process.
//!
//! This module depends on `polaris-auth` — a deliberate, narrow exception
//! to the boundary the rest of `polaris-tui` keeps (see `lib.rs`'s
//! `RunArgs` doc comment): onboarding is inherently a rendering+auth
//! operation woven together turn by turn, unlike the chat loop, which
//! cleanly keeps "build credentials" (polaris-cli) and "render/loop"
//! (polaris-tui) separate.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    ChatGpt,
    ApiKey,
}

impl Choice {
    pub fn toggled(self) -> Choice {
        match self {
            Choice::ChatGpt => Choice::ApiKey,
            Choice::ApiKey => Choice::ChatGpt,
        }
    }
}

/// One numbered, two-line option: a marker + title row (highlighted when
/// selected) and a dim subtitle row underneath, mirroring the layout of
/// codex's own first-run screen without reusing its wording.
fn option_lines(
    number: u8,
    choice: Choice,
    selected: Choice,
    title: &str,
    subtitle: &str,
) -> [Line<'static>; 2] {
    let is_selected = choice == selected;
    let marker = if is_selected { "> " } else { "  " };
    let title_style = if is_selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    [
        Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{number}. {title}"), title_style),
        ]),
        Line::styled(
            format!("     {subtitle}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

pub fn render_choice_screen(frame: &mut Frame, selected: Choice) {
    let area = frame.area();

    let mut lines = vec![
        Line::styled(
            "Welcome to polaris, an improved harness built on codex",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("Sign in with ChatGPT to use polaris with your ChatGPT"),
        Line::from("subscription, or provide your own OpenAI API key for"),
        Line::from("usage-based billing."),
        Line::from(""),
    ];
    lines.extend(option_lines(
        1,
        Choice::ChatGpt,
        selected,
        "Sign in with ChatGPT",
        "Uses your existing ChatGPT Plus, Pro, Business, or Enterprise plan",
    ));
    lines.push(Line::from(""));
    lines.extend(option_lines(
        2,
        Choice::ApiKey,
        selected,
        "Provide your own API key",
        "Pay for what you use",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "\u{2191}/\u{2193} or 1/2 to choose, Enter to continue",
    ));

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

pub fn render_api_key_prompt(frame: &mut Frame, typed_len: usize) {
    let area = frame.area();
    let [prompt_area, input_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).areas(area);
    frame.render_widget(
        Paragraph::new("Paste or type your OpenAI API key, then press Enter."),
        prompt_area,
    );
    let masked: String = "*".repeat(typed_len);
    frame.render_widget(
        Paragraph::new(masked).block(Block::default().borders(Borders::ALL).title("API key")),
        input_area,
    );
}

pub enum ChoiceAction {
    Continue,
    Toggle,
    Submit,
    Quit,
}

/// Applies one key event on the choice screen. Up/Down/1/2 toggle between
/// the two choices (there are only two, so any "change selection" key
/// just flips it); Enter submits the currently selected choice.
pub fn apply_choice_key(key: KeyEvent) -> ChoiceAction {
    if key.kind != KeyEventKind::Press {
        return ChoiceAction::Continue;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return ChoiceAction::Quit;
    }
    match key.code {
        KeyCode::Up | KeyCode::Down | KeyCode::Char('1') | KeyCode::Char('2') => {
            ChoiceAction::Toggle
        }
        KeyCode::Enter => ChoiceAction::Submit,
        _ => ChoiceAction::Continue,
    }
}

pub enum KeyEntryAction {
    Continue,
    Submit(String),
    Quit,
}

/// Applies one key event on the API-key entry screen. Mirrors
/// `input::apply_key`'s buffer-editing shape (char accumulates, Backspace
/// removes, Enter submits and clears), but keeps the "submit an empty
/// buffer is a no-op" behavior too — a blank Enter shouldn't silently save
/// an empty key.
pub fn apply_key_entry_key(buffer: &mut String, key: KeyEvent) -> KeyEntryAction {
    if key.kind != KeyEventKind::Press {
        return KeyEntryAction::Continue;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyEntryAction::Quit;
    }
    match key.code {
        KeyCode::Enter => {
            if buffer.trim().is_empty() {
                KeyEntryAction::Continue
            } else {
                KeyEntryAction::Submit(std::mem::take(buffer).trim().to_string())
            }
        }
        KeyCode::Backspace => {
            buffer.pop();
            KeyEntryAction::Continue
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            KeyEntryAction::Continue
        }
        _ => KeyEntryAction::Continue,
    }
}

use std::io::IsTerminal;
use std::path::Path;

/// Which credential became available once onboarding completes
/// successfully. `polaris-cli::main()` uses this to decide which
/// provider arm to retry.
#[derive(Debug)]
pub enum Outcome {
    ApiKeySaved,
    CodexLoggedIn,
}

#[derive(Debug)]
pub enum OnboardingError {
    NonInteractive,
    Cancelled,
    Auth(String),
}

impl std::fmt::Display for OnboardingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OnboardingError::NonInteractive => {
                write!(
                    f,
                    "polaris: refusing to start onboarding on a non-interactive terminal"
                )
            }
            OnboardingError::Cancelled => write!(f, "onboarding cancelled"),
            OnboardingError::Auth(msg) => write!(f, "{msg}"),
        }
    }
}

/// Shows the onboarding screen and drives it to completion. Owns the
/// terminal for its own lifetime (enters/leaves the alternate screen
/// itself via `ratatui::init()`/`ratatui::restore()`), independent of
/// whatever the caller does with the terminal afterward — the chat loop's
/// own `run()` re-inits the terminal fresh once this returns.
pub async fn run(auth_store_path: &Path, api_key_path: &Path) -> Result<Outcome, OnboardingError> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(OnboardingError::NonInteractive);
    }

    let mut terminal = ratatui::init();
    let mut selected = Choice::ChatGpt;

    let outcome = 'outer: loop {
        if terminal
            .draw(|f| render_choice_screen(f, selected))
            .is_err()
        {
            break 'outer Err(OnboardingError::Auth(
                "failed to draw the terminal".to_string(),
            ));
        }
        let event = match ratatui::crossterm::event::read() {
            Ok(e) => e,
            Err(_) => break 'outer Err(OnboardingError::Auth("failed to read a key".to_string())),
        };
        let ratatui::crossterm::event::Event::Key(key) = event else {
            continue;
        };

        match apply_choice_key(key) {
            ChoiceAction::Continue => continue,
            ChoiceAction::Toggle => {
                selected = selected.toggled();
                continue;
            }
            ChoiceAction::Quit => break 'outer Err(OnboardingError::Cancelled),
            ChoiceAction::Submit => {}
        }

        match selected {
            Choice::ChatGpt => {
                // Leave the alternate screen before calling into the
                // existing OAuth flow: `login::run` prints the
                // authorize-in-your-browser URL via `eprintln!` and opens
                // a browser, neither of which should happen while the TUI
                // owns the terminal.
                ratatui::restore();
                let result = polaris_auth::login::run(polaris_auth::ISSUER, auth_store_path).await;
                terminal = ratatui::init();
                match result {
                    Ok(_) => break 'outer Ok(Outcome::CodexLoggedIn),
                    Err(e) => {
                        // Stay on the choice screen and let the user try
                        // again rather than exiting the whole process over
                        // one failed sign-in attempt (e.g. the user closed
                        // the browser tab without authorizing).
                        if terminal
                            .draw(|f| {
                                let area = f.area();
                                f.render_widget(
                                    ratatui::widgets::Paragraph::new(format!(
                                        "sign-in failed: {e}\n\npress any key to try again"
                                    )),
                                    area,
                                )
                            })
                            .is_err()
                        {
                            break 'outer Err(OnboardingError::Auth(e.to_string()));
                        }
                        let _ = ratatui::crossterm::event::read();
                        continue;
                    }
                }
            }
            Choice::ApiKey => {
                let mut typed = String::new();
                let key = 'entry: loop {
                    if terminal
                        .draw(|f| render_api_key_prompt(f, typed.chars().count()))
                        .is_err()
                    {
                        break 'outer Err(OnboardingError::Auth(
                            "failed to draw the terminal".to_string(),
                        ));
                    }
                    let event = match ratatui::crossterm::event::read() {
                        Ok(e) => e,
                        Err(_) => {
                            break 'outer Err(OnboardingError::Auth(
                                "failed to read a key".to_string(),
                            ));
                        }
                    };
                    let ratatui::crossterm::event::Event::Key(key_event) = event else {
                        continue;
                    };
                    match apply_key_entry_key(&mut typed, key_event) {
                        KeyEntryAction::Continue => continue,
                        KeyEntryAction::Quit => break 'outer Err(OnboardingError::Cancelled),
                        KeyEntryAction::Submit(key) => break 'entry key,
                    }
                };
                match polaris_auth::api_key::save_to(api_key_path, &key) {
                    Ok(()) => break 'outer Ok(Outcome::ApiKeySaved),
                    Err(e) => break 'outer Err(OnboardingError::Auth(e.to_string())),
                }
            }
        }
    };

    ratatui::restore();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn the_choice_screen_marks_the_selected_option() {
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_choice_screen(f, Choice::ChatGpt))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("Welcome to polaris"));
        assert!(content.contains("Sign in with ChatGPT"));
        assert!(content.contains("Provide your own API key"));
        assert!(content.contains("Pay for what you use"));
    }

    #[test]
    fn the_choice_screen_highlights_whichever_option_is_selected() {
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_choice_screen(f, Choice::ApiKey))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let find_row = |needle: &str| {
            for y in 0..buffer.area.height {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if let Some(x) = row.find(needle) {
                    return Some(buffer[(x as u16, y)].fg);
                }
            }
            None
        };
        assert_eq!(
            find_row("2. Provide your own API key"),
            Some(Color::Cyan),
            "the selected option's marker column should be highlighted"
        );
        assert_ne!(
            find_row("1. Sign in with ChatGPT"),
            Some(Color::Cyan),
            "the unselected option should not be highlighted"
        );
    }

    #[test]
    fn toggled_switches_between_the_two_choices() {
        assert_eq!(Choice::ChatGpt.toggled(), Choice::ApiKey);
        assert_eq!(Choice::ApiKey.toggled(), Choice::ChatGpt);
    }

    #[test]
    fn the_api_key_prompt_never_shows_the_real_characters() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_api_key_prompt(f, 5))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("*****"));
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn up_down_and_digit_keys_all_toggle_the_choice() {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('1'),
            KeyCode::Char('2'),
        ] {
            assert!(matches!(
                apply_choice_key(press(code)),
                ChoiceAction::Toggle
            ));
        }
    }

    #[test]
    fn enter_submits_the_choice_screen() {
        assert!(matches!(
            apply_choice_key(press(KeyCode::Enter)),
            ChoiceAction::Submit
        ));
    }

    #[test]
    fn ctrl_c_quits_the_choice_screen() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(apply_choice_key(ctrl_c), ChoiceAction::Quit));
    }

    #[test]
    fn typed_characters_accumulate_in_the_key_entry_buffer() {
        let mut buffer = String::new();
        apply_key_entry_key(&mut buffer, press(KeyCode::Char('s')));
        apply_key_entry_key(&mut buffer, press(KeyCode::Char('k')));
        assert_eq!(buffer, "sk");
    }

    #[test]
    fn backspace_removes_the_last_character_from_the_key_entry_buffer() {
        let mut buffer = "sk-x".to_string();
        apply_key_entry_key(&mut buffer, press(KeyCode::Backspace));
        assert_eq!(buffer, "sk-");
    }

    #[test]
    fn enter_submits_and_clears_a_nonempty_key_entry_buffer() {
        let mut buffer = "sk-example".to_string();
        let action = apply_key_entry_key(&mut buffer, press(KeyCode::Enter));
        assert!(buffer.is_empty());
        match action {
            KeyEntryAction::Submit(key) => assert_eq!(key, "sk-example"),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn enter_on_an_empty_key_entry_buffer_does_nothing() {
        let mut buffer = String::new();
        let action = apply_key_entry_key(&mut buffer, press(KeyCode::Enter));
        assert!(matches!(action, KeyEntryAction::Continue));
    }

    #[test]
    fn ctrl_c_quits_the_key_entry_screen() {
        let mut buffer = "partial".to_string();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            apply_key_entry_key(&mut buffer, ctrl_c),
            KeyEntryAction::Quit
        ));
    }

    #[tokio::test]
    async fn a_non_interactive_terminal_is_refused_immediately() {
        // Cargo may inherit a real terminal. Run this test in a child with
        // explicit non-terminal streams, never against the user's TTY.
        const CHILD: &str = "POLARIS_TEST_NON_INTERACTIVE_ONBOARDING";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "onboarding::tests::a_non_interactive_terminal_is_refused_immediately",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .expect("run non-interactive child");
            assert!(
                output.status.success(),
                "child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert!(!std::io::stdin().is_terminal());
        assert!(!std::io::stdout().is_terminal());
        let dir = tempfile::tempdir().expect("temp dir");
        let auth_path = dir.path().join("auth.json");
        let key_path = dir.path().join("api_key.json");

        let err = run(&auth_path, &key_path)
            .await
            .expect_err("a non-interactive terminal must be refused");
        assert!(matches!(err, OnboardingError::NonInteractive));
        assert!(!auth_path.exists());
        assert!(!key_path.exists());
    }
}
