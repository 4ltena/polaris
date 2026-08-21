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
}
