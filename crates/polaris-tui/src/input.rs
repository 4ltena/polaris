//! Pure keystroke-to-action mapping for the input box. Kept separate from
//! `run()` so the editing rules (what Enter/Backspace/Ctrl-C do) are
//! testable without a real terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub enum InputAction {
    Continue,
    Submit(String),
    Quit,
}

/// Applies one key event to the input buffer, returning what the caller
/// should do next. Mutates `buffer` in place for `Char`/`Backspace`.
///
/// On Windows consoles, and with the kitty keyboard protocol on some Unix
/// terminals, crossterm can deliver both a Press and a Release event for
/// the same physical keystroke. Only Press is acted on here, or every
/// typed character would double and Enter would submit twice.
pub fn apply_key(buffer: &mut String, key: KeyEvent) -> InputAction {
    if key.kind != KeyEventKind::Press {
        return InputAction::Continue;
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return InputAction::Quit;
    }

    match key.code {
        KeyCode::Enter => {
            let text = std::mem::take(buffer);
            InputAction::Submit(text)
        }
        KeyCode::Backspace => {
            buffer.pop();
            InputAction::Continue
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            InputAction::Continue
        }
        _ => InputAction::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typed_characters_accumulate_in_the_buffer() {
        let mut buffer = String::new();
        apply_key(&mut buffer, key(KeyCode::Char('h')));
        apply_key(&mut buffer, key(KeyCode::Char('i')));
        assert_eq!(buffer, "hi");
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut buffer = "hi".to_string();
        apply_key(&mut buffer, key(KeyCode::Backspace));
        assert_eq!(buffer, "h");
    }

    #[test]
    fn enter_submits_and_clears_the_buffer() {
        let mut buffer = "hello".to_string();
        let action = apply_key(&mut buffer, key(KeyCode::Enter));
        assert!(buffer.is_empty());
        match action {
            InputAction::Submit(text) => assert_eq!(text, "hello"),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn ctrl_c_quits_regardless_of_buffer_contents() {
        let mut buffer = "unfinished".to_string();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = apply_key(&mut buffer, ctrl_c);
        assert!(matches!(action, InputAction::Quit));
    }

    #[test]
    fn a_key_release_event_is_ignored() {
        let mut buffer = String::new();
        let mut release = key(KeyCode::Char('h'));
        release.kind = KeyEventKind::Release;
        let action = apply_key(&mut buffer, release);
        assert!(buffer.is_empty());
        assert!(matches!(action, InputAction::Continue));
    }

    #[test]
    fn a_key_repeat_event_is_ignored() {
        let mut buffer = String::new();
        let mut repeat = key(KeyCode::Char('h'));
        repeat.kind = KeyEventKind::Repeat;
        let action = apply_key(&mut buffer, repeat);
        assert!(buffer.is_empty());
        assert!(matches!(action, InputAction::Continue));
    }
}
