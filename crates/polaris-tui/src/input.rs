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
/// should do next. Mutates `buffer` in place for `Char`/`Backspace`/`Left`/
/// `Right`, and keeps `cursor` — a byte offset into `buffer`, always on a
/// UTF-8 char boundary — in sync with it.
///
/// On Windows consoles, and with the kitty keyboard protocol on some Unix
/// terminals, crossterm can deliver both a Press and a Release event for
/// the same physical keystroke. Only Press is acted on here, or every
/// typed character would double and Enter would submit twice.
pub fn apply_key(buffer: &mut String, cursor: &mut usize, key: KeyEvent) -> InputAction {
    if key.kind != KeyEventKind::Press {
        return InputAction::Continue;
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return InputAction::Quit;
    }

    match key.code {
        KeyCode::Enter => {
            let text = std::mem::take(buffer);
            *cursor = 0;
            InputAction::Submit(text)
        }
        KeyCode::Backspace => {
            if let Some(prev) = prev_char_boundary(buffer, *cursor) {
                buffer.replace_range(prev..*cursor, "");
                *cursor = prev;
            }
            InputAction::Continue
        }
        KeyCode::Left => {
            if let Some(prev) = prev_char_boundary(buffer, *cursor) {
                *cursor = prev;
            }
            InputAction::Continue
        }
        KeyCode::Right => {
            if let Some(next) = next_char_boundary(buffer, *cursor) {
                *cursor = next;
            }
            InputAction::Continue
        }
        KeyCode::Char(c) => {
            buffer.insert(*cursor, c);
            *cursor += c.len_utf8();
            InputAction::Continue
        }
        _ => InputAction::Continue,
    }
}

fn prev_char_boundary(buffer: &str, cursor: usize) -> Option<usize> {
    buffer[..cursor].char_indices().next_back().map(|(i, _)| i)
}

fn next_char_boundary(buffer: &str, cursor: usize) -> Option<usize> {
    buffer[cursor..]
        .chars()
        .next()
        .map(|c| cursor + c.len_utf8())
}

/// Session-local recall of previously submitted input, mirroring a shell's
/// Up/Down history. Not persisted to disk — a fresh `History` is created
/// each time the TUI starts.
pub struct History {
    entries: Vec<String>,
    /// Index into `entries` currently shown, or `None` when showing the
    /// live draft (not navigating history).
    index: Option<usize>,
    /// The buffer's content at the moment `older` first left it, restored
    /// when `newer` returns past the newest entry.
    draft: String,
}

impl History {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: None,
            draft: String::new(),
        }
    }

    /// Records a submitted line. Empty text and immediate repeats of the
    /// last entry are not recorded (matching typical shell history).
    /// Resets navigation back to the live draft.
    pub fn record(&mut self, text: &str) {
        if !text.is_empty() && self.entries.last().map(String::as_str) != Some(text) {
            self.entries.push(text.to_string());
        }
        self.index = None;
        self.draft.clear();
    }

    /// Moves one entry older. `current` is the live buffer's current
    /// content, saved as the draft the first time navigation leaves it.
    /// Returns the entry to show, or `None` if already at the oldest entry
    /// (or there is no history) — a no-op either way.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        match self.index {
            None => {
                if self.entries.is_empty() {
                    return None;
                }
                self.draft = current.to_string();
                self.index = Some(self.entries.len() - 1);
            }
            Some(0) => return None,
            Some(i) => self.index = Some(i - 1),
        }
        self.index.map(|i| self.entries[i].as_str())
    }

    /// Moves one entry newer, or back to the live draft when already at the
    /// newest entry. Returns `None` if already at the live draft (a no-op).
    pub fn newer(&mut self) -> Option<&str> {
        match self.index {
            None => None,
            Some(i) if i + 1 < self.entries.len() => {
                self.index = Some(i + 1);
                Some(self.entries[i + 1].as_str())
            }
            Some(_) => {
                self.index = None;
                Some(self.draft.as_str())
            }
        }
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
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
        let mut cursor = 0;
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Char('h')));
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Char('i')));
        assert_eq!(buffer, "hi");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn left_then_typing_inserts_before_the_last_character() {
        let mut buffer = "hi".to_string();
        let mut cursor = 2;
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Left));
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Char('X')));
        assert_eq!(buffer, "hXi");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn left_does_not_move_past_the_start() {
        let mut buffer = "hi".to_string();
        let mut cursor = 0;
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Left));
        assert_eq!(cursor, 0);
    }

    #[test]
    fn right_does_not_move_past_the_end() {
        let mut buffer = "hi".to_string();
        let mut cursor = 2;
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Right));
        assert_eq!(cursor, 2);
    }

    #[test]
    fn left_right_step_by_whole_multibyte_characters() {
        let mut buffer = "a\u{3042}b".to_string(); // "a" + hiragana "あ" + "b"
        let mut cursor = buffer.len();
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Left));
        assert_eq!(cursor, "a\u{3042}".len());
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Left));
        assert_eq!(cursor, "a".len());
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Right));
        assert_eq!(cursor, "a\u{3042}".len());
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor_not_always_the_last() {
        let mut buffer = "hi".to_string();
        let mut cursor = 1; // between 'h' and 'i'
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Backspace));
        assert_eq!(buffer, "i");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut buffer = "hi".to_string();
        let mut cursor = 0;
        apply_key(&mut buffer, &mut cursor, key(KeyCode::Backspace));
        assert_eq!(buffer, "hi");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn enter_submits_and_clears_the_buffer_and_resets_the_cursor() {
        let mut buffer = "hello".to_string();
        let mut cursor = 5;
        let action = apply_key(&mut buffer, &mut cursor, key(KeyCode::Enter));
        assert!(buffer.is_empty());
        assert_eq!(cursor, 0);
        match action {
            InputAction::Submit(text) => assert_eq!(text, "hello"),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn ctrl_c_quits_regardless_of_buffer_contents() {
        let mut buffer = "unfinished".to_string();
        let mut cursor = buffer.len();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = apply_key(&mut buffer, &mut cursor, ctrl_c);
        assert!(matches!(action, InputAction::Quit));
    }

    #[test]
    fn a_key_release_event_is_ignored() {
        let mut buffer = String::new();
        let mut cursor = 0;
        let mut release = key(KeyCode::Char('h'));
        release.kind = KeyEventKind::Release;
        let action = apply_key(&mut buffer, &mut cursor, release);
        assert!(buffer.is_empty());
        assert!(matches!(action, InputAction::Continue));
    }

    #[test]
    fn a_key_repeat_event_is_ignored() {
        let mut buffer = String::new();
        let mut cursor = 0;
        let mut repeat = key(KeyCode::Char('h'));
        repeat.kind = KeyEventKind::Repeat;
        let action = apply_key(&mut buffer, &mut cursor, repeat);
        assert!(buffer.is_empty());
        assert!(matches!(action, InputAction::Continue));
    }

    #[test]
    fn history_older_returns_none_when_empty() {
        let mut history = History::new();
        assert_eq!(history.older("draft"), None);
    }

    #[test]
    fn history_older_recalls_the_most_recent_entry_first() {
        let mut history = History::new();
        history.record("first");
        history.record("second");
        assert_eq!(history.older(""), Some("second"));
    }

    #[test]
    fn history_older_does_not_move_past_the_oldest_entry() {
        let mut history = History::new();
        history.record("only");
        assert_eq!(history.older(""), Some("only"));
        assert_eq!(history.older(""), None);
    }

    #[test]
    fn history_newer_past_the_newest_entry_restores_the_saved_draft() {
        let mut history = History::new();
        history.record("first");
        assert_eq!(history.older("unsent draft"), Some("first"));
        assert_eq!(history.newer(), Some("unsent draft"));
    }

    #[test]
    fn history_newer_is_a_no_op_when_not_navigating() {
        let mut history = History::new();
        history.record("first");
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn history_record_ignores_empty_text() {
        let mut history = History::new();
        history.record("");
        assert_eq!(history.older(""), None);
    }

    #[test]
    fn history_record_ignores_an_immediate_repeat() {
        let mut history = History::new();
        history.record("same");
        history.record("same");
        assert_eq!(history.older(""), Some("same"));
        assert_eq!(history.older(""), None);
    }
}
