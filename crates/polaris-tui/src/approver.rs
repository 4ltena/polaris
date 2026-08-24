//! The `Approver` that runs inside the TUI: draws a modal over the current
//! frame and blocks on a single keypress. Generic over `KeyReader` so
//! tests can feed keys without a real terminal.

use std::cell::RefCell;

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

/// `terminal` is shared via `RefCell`, not held exclusively, because the
/// main loop's agent turn now redraws the screen concurrently with
/// `agent::run` (see `lib.rs`'s `tokio::select!` around the turn) —
/// something that borrows the same terminal to animate the status row
/// while `agent::run` is in flight, including while it's inside a call to
/// `ask` here. Both sides only ever borrow it for the duration of one
/// synchronous `draw` call, never across an `.await`, so runtime aliasing
/// never actually occurs even though the type allows it.
pub struct TuiApprover<'a, B: Backend, R: KeyReader> {
    pub terminal: &'a RefCell<Terminal<B>>,
    pub reader: &'a mut R,
}

impl<'a, B: Backend, R: KeyReader> Approver for TuiApprover<'a, B, R> {
    fn ask(&mut self, reason: &str) -> Decision {
        // A draw failure or a read failure both fall through to Deny — an
        // approval gate that silently allows on I/O trouble is not a gate.
        if self
            .terminal
            .borrow_mut()
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
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        let mut approver = TuiApprover {
            terminal: &terminal,
            reader: &mut reader,
        };

        assert_eq!(approver.ask("writing to src/main.rs"), Decision::Allow);
    }

    #[test]
    fn n_denies() {
        let backend = TestBackend::new(40, 10);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('n')]));
        let mut approver = TuiApprover {
            terminal: &terminal,
            reader: &mut reader,
        };

        assert_eq!(approver.ask("writing to src/main.rs"), Decision::Deny);
    }

    #[test]
    fn an_unrecognized_key_is_ignored_until_y_or_n_comes() {
        let backend = TestBackend::new(40, 10);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([
            KeyCode::Char('x'),
            KeyCode::Up,
            KeyCode::Char('y'),
        ]));
        let mut approver = TuiApprover {
            terminal: &terminal,
            reader: &mut reader,
        };

        assert_eq!(approver.ask("writing to src/main.rs"), Decision::Allow);
    }
}
