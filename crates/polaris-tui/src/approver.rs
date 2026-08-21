//! The `Approver` that runs inside the TUI: draws a modal over the current
//! frame and blocks on a single keypress. Generic over `KeyReader` so
//! tests can feed keys without a real terminal.

use polaris_core::approval::{Approver, Decision};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{KeyCode, KeyEventKind};

use crate::render::render_approval_modal;

/// Abstracts "block until the next key comes in" so `TuiApprover` can be
/// tested without a real terminal.
pub trait KeyReader {
    fn read_key(&mut self) -> std::io::Result<KeyCode>;
}

/// The real, blocking reader used outside tests.
pub struct CrosstermKeyReader;

impl KeyReader for CrosstermKeyReader {
    fn read_key(&mut self) -> std::io::Result<KeyCode> {
        loop {
            if let ratatui::crossterm::event::Event::Key(k) = ratatui::crossterm::event::read()?
                && k.kind == KeyEventKind::Press
            {
                return Ok(k.code);
            }
        }
    }
}

pub struct TuiApprover<'a, B: Backend, R: KeyReader> {
    pub terminal: &'a mut Terminal<B>,
    pub reader: &'a mut R,
}

impl<'a, B: Backend, R: KeyReader> Approver for TuiApprover<'a, B, R> {
    fn ask(&mut self, reason: &str) -> Decision {
        // A draw failure or a read failure both fall through to Deny — an
        // approval gate that silently allows on I/O trouble is not a gate.
        if self
            .terminal
            .draw(|f| render_approval_modal(f, reason))
            .is_err()
        {
            return Decision::Deny;
        }

        loop {
            match self.reader.read_key() {
                Ok(KeyCode::Char('y')) | Ok(KeyCode::Char('Y')) => return Decision::Allow,
                Ok(KeyCode::Char('n')) | Ok(KeyCode::Char('N')) => return Decision::Deny,
                Ok(_) => continue,
                Err(_) => return Decision::Deny,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;

    struct ScriptedReader(VecDeque<KeyCode>);

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0.pop_front().unwrap_or(KeyCode::Null))
        }
    }

    #[test]
    fn y_allows() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        let mut approver = TuiApprover {
            terminal: &mut terminal,
            reader: &mut reader,
        };

        assert_eq!(approver.ask("writing to src/main.rs"), Decision::Allow);
    }

    #[test]
    fn n_denies() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('n')]));
        let mut approver = TuiApprover {
            terminal: &mut terminal,
            reader: &mut reader,
        };

        assert_eq!(approver.ask("writing to src/main.rs"), Decision::Deny);
    }

    #[test]
    fn an_unrecognized_key_is_ignored_until_y_or_n_comes() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut reader = ScriptedReader(VecDeque::from([
            KeyCode::Char('x'),
            KeyCode::Up,
            KeyCode::Char('y'),
        ]));
        let mut approver = TuiApprover {
            terminal: &mut terminal,
            reader: &mut reader,
        };

        assert_eq!(approver.ask("writing to src/main.rs"), Decision::Allow);
    }
}
