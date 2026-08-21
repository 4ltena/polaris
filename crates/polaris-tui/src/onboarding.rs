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
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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

pub fn render_choice_screen(frame: &mut Frame, selected: Choice) {
    let area = frame.area();
    let marker = |c: Choice| if c == selected { ">" } else { " " };
    let text = format!(
        "polaris — a minimal-context coding agent harness\n\n\
         Sign in to continue.\n\n\
         {} 1. Sign in with ChatGPT\n\
         {} 2. Provide an OpenAI API key\n\n\
         Use \u{2191}/\u{2193} or 1/2, then Enter.",
        marker(Choice::ChatGpt),
        marker(Choice::ApiKey),
    );
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("polaris")),
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
                KeyEntryAction::Submit(std::mem::take(buffer))
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn the_choice_screen_marks_the_selected_option() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_choice_screen(f, Choice::ChatGpt))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("Sign in with ChatGPT"));
        assert!(content.contains("Provide an OpenAI API key"));
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

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("*****"));
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn up_down_and_digit_keys_all_toggle_the_choice() {
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Char('1'), KeyCode::Char('2')] {
            assert!(matches!(apply_choice_key(press(code)), ChoiceAction::Toggle));
        }
    }

    #[test]
    fn enter_submits_the_choice_screen() {
        assert!(matches!(apply_choice_key(press(KeyCode::Enter)), ChoiceAction::Submit));
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
        assert!(matches!(apply_key_entry_key(&mut buffer, ctrl_c), KeyEntryAction::Quit));
    }
}
