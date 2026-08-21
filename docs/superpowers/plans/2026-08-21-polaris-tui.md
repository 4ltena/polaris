# polaris TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an interactive, persistent TUI to `polaris` (entered by running `polaris` without `--prompt`), reusing the existing `Session`/`agent::run`/`Approver` machinery without pulling in any `codex-*` crate.

**Architecture:** A new `polaris-tui` crate holds a synchronous, blocking event loop (no background tasks, no channels) built on `ratatui` (crossterm backend, via `ratatui`'s own re-export — no separate `crossterm` dependency). `polaris-cli`'s `main()` builds the provider/sandbox/skills/always-on context exactly as it does today, then branches: `--prompt` given → today's one-shot path (unchanged); `--prompt` omitted → calls `polaris_tui::run(RunArgs { .. })`, which owns the terminal, the `Session`, and the turn loop for the rest of the process's life.

**Tech Stack:** Rust 1.96, edition 2024, `ratatui` (crossterm backend), existing `polaris-core`/`polaris-provider`/`polaris-sandbox`/`polaris-skills`.

**Spec:** `docs/superpowers/specs/2026-08-21-polaris-tui-design.md`

## Global Constraints

- Rust 1.96.0, edition 2024 (`rust-toolchain.toml`, `workspace.package`).
- New crate must set `license.workspace = true` (dual `MIT OR Apache-2.0`), matching every existing crate.
- No `codex-*` crate may appear as a dependency anywhere in this plan — see the spec's rationale.
- `polaris_core::prompt::assemble_always_on` and its output (`AlwaysOn`) must not be modified or re-assembled by TUI code — it is built once by `polaris-cli::main()` exactly as today and passed in by reference, so the 990-token always-on budget invariant (`crates/polaris-core/src/budget.rs`) stays intact.
- Session file format: one `polaris_provider::Message` (already `Serialize`/`Deserialize`) per line, newline-delimited JSON, at `<state-dir>/tui-session.jsonl`.
- `crates/polaris-core/tests/filemap.rs` asserts `docs/filemap.md` matches the repository. Any task that adds/removes a `.rs` file must end with `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` before committing, or the workspace test suite will fail for everyone after.

---

### Task 1: Move `project_id`/state-dir helpers into `polaris_core::project`

**Files:**
- Modify: `crates/polaris-core/src/project.rs`
- Modify: `crates/polaris-cli/src/main.rs:475-518` (the `default_state_dir`, `default_audit_path`, `project_id` functions and their tests)

**Interfaces:**
- Produces: `polaris_core::project::project_id(canonical_path: &Path) -> String`, `polaris_core::project::state_dir(cwd: &Path) -> std::io::Result<PathBuf>` — both `pub`. `polaris-tui` (Task 2 onward) needs these to place `tui-session.jsonl` next to the existing `audit.jsonl`, and they currently live as private `fn`s in `polaris-cli`, which `polaris-tui` cannot reach as a sibling crate.

Today `default_state_dir`/`project_id` are private functions in `polaris-cli/src/main.rs`. Move them (body unchanged) into `polaris_core::project` as `state_dir`/`project_id`, then have `main.rs` call the moved versions.

- [ ] **Step 1: Move the two functions and their tests into `project.rs`**

Append to `crates/polaris-core/src/project.rs` (before the existing `#[cfg(test)] mod tests` block's closing brace — i.e. add the functions above the test module, and the new tests inside it, next to the existing `resolve_root` tests):

```rust
/// Builds a deterministic project identifier from a canonicalized path.
///
/// We don't use the standard library's `DefaultHasher`, since it doesn't
/// specify its algorithm and can change across Rust versions (which would
/// split the same project's audit log into a different directory). We
/// write out FNV-1a directly instead, to pin the algorithm.
pub fn project_id(canonical_path: &Path) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical_path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// The project's state directory: `~/.polaris/state/<project-id>/`, created
/// if missing. `<project-id>` is derived from the resolved project root
/// (`resolve_root`), not the working directory — see `project_id`'s docs on
/// why launch location must not split a project's state across directories.
pub fn state_dir(cwd: &Path) -> std::io::Result<PathBuf> {
    let root = resolve_root(cwd);
    let id = project_id(&root);

    let home = std::env::var_os("HOME").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set")
    })?;
    let dir = Path::new(&home).join(".polaris").join("state").join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
```

Add inside the existing `mod tests` block in `project.rs`:

```rust
    #[test]
    fn project_id_is_deterministic_for_the_same_path() {
        let a = project_id(Path::new("/w/polaris"));
        let b = project_id(Path::new("/w/polaris"));
        assert_eq!(a, b);
    }

    #[test]
    fn project_id_differs_for_different_paths() {
        let a = project_id(Path::new("/w/polaris"));
        let b = project_id(Path::new("/w/other"));
        assert_ne!(a, b);
    }
```

- [ ] **Step 2: Delete the moved code from `main.rs` and call through `polaris_core::project`**

In `crates/polaris-cli/src/main.rs`, delete the `default_state_dir`, `project_id` functions and the two `project_id_*` tests (lines 475-518 and their matching `#[test]` bodies under `mod tests`), and change every call site:

```rust
// before
let state_dir = match default_state_dir(&cwd) {
// after
let state_dir = match polaris_core::project::state_dir(&cwd) {
```

Keep `default_audit_path` in `main.rs` as-is (it's a one-line wrapper — `Ok(default_state_dir(cwd)?.join("audit.jsonl"))` — just repoint its body at `polaris_core::project::state_dir`).

- [ ] **Step 3: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass, including the two relocated `project_id_*` tests now running from `polaris-core`.

- [ ] **Step 4: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-core/src/project.rs crates/polaris-cli/src/main.rs docs/filemap.md
git commit -m "refactor: move project-id/state-dir helpers into polaris_core::project"
```

---

### Task 2: Scaffold `polaris-tui` and its session persistence

**Files:**
- Create: `crates/polaris-tui/Cargo.toml`
- Create: `crates/polaris-tui/src/lib.rs`
- Create: `crates/polaris-tui/src/persist.rs`
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/polaris-cli/Cargo.toml` (add `polaris-tui` dependency)

**Interfaces:**
- Consumes: `polaris_core::session::Session` (`pub struct Session { pub messages: Vec<Message> }`, `Default`), `polaris_provider::Message` (`Serialize`/`Deserialize`).
- Produces: `polaris_tui::persist::load_session(path: &Path) -> std::io::Result<(Session, bool)>` (the `bool` is `true` when a corrupt line was found, truncated, and the file rewritten without it), `polaris_tui::persist::append_message(path: &Path, message: &Message) -> std::io::Result<()>`. Task 5's `run()` loop calls both.

- [ ] **Step 1: Add `ratatui` to the workspace and create the crate manifest**

In `Cargo.toml`, add to `[workspace.dependencies]`:

```toml
ratatui = "0.29"
```

Create `crates/polaris-tui/Cargo.toml`:

```toml
[package]
name = "polaris-tui"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
polaris-core = { path = "../polaris-core" }
polaris-provider = { path = "../polaris-provider" }
ratatui = { workspace = true }
serde_json = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 2: Write the failing persistence tests**

Create `crates/polaris-tui/src/persist.rs`:

```rust
//! Session persistence: one JSON `Message` per line.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use polaris_core::session::Session;
use polaris_provider::Message;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_loads_as_an_empty_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        let (session, truncated) = load_session(&path).expect("load");

        assert!(session.messages.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn appended_messages_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        append_message(&path, &Message::user("hi")).expect("append");
        append_message(&path, &Message::assistant("hello")).expect("append");

        let (session, truncated) = load_session(&path).expect("load");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "hi");
        assert_eq!(session.messages[1].content, "hello");
        assert!(!truncated);
    }

    #[test]
    fn a_corrupt_line_truncates_and_rewrites_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        append_message(&path, &Message::user("good line")).expect("append");
        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        writeln!(file, "{{not valid json").expect("write corrupt line");
        drop(file);
        append_message(&path, &Message::user("orphaned, after the corrupt line")).expect("append");

        let (session, truncated) = load_session(&path).expect("load");

        assert!(truncated);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "good line");

        // Reloading again must be clean now that the corrupt tail was rewritten away.
        let (reloaded, truncated_again) = load_session(&path).expect("reload");
        assert_eq!(reloaded.messages.len(), 1);
        assert!(!truncated_again);
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `load_session`/`append_message` not defined yet.

- [ ] **Step 4: Implement `load_session` and `append_message`**

Add above the `#[cfg(test)]` line in the same file:

```rust
/// Loads a session from `path`. Returns `(session, true)` when a corrupt
/// line was found; everything from that line onward is dropped both from
/// the returned `Session` and from the file on disk, so a later reload
/// (or a later `append_message`) never re-encounters it. A missing file
/// loads as an empty session, not an error — there is nothing to resume
/// yet on first run.
pub fn load_session(path: &Path) -> io::Result<(Session, bool)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Session::default(), false)),
        Err(e) => return Err(e),
    };

    let mut messages = Vec::new();
    let mut truncated = false;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(&line) {
            Ok(m) => messages.push(m),
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }

    if truncated {
        rewrite(path, &messages)?;
    }

    Ok((Session { messages }, truncated))
}

/// Appends one message as a single JSON line. Creates the file if it
/// doesn't exist yet.
pub fn append_message(path: &Path, message: &Message) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(message).expect("Message always serializes");
    writeln!(file, "{line}")
}

fn rewrite(path: &Path, messages: &[Message]) -> io::Result<()> {
    let mut file = File::create(path)?;
    for m in messages {
        let line = serde_json::to_string(m).expect("Message always serializes");
        writeln!(file, "{line}")?;
    }
    Ok(())
}
```

Create `crates/polaris-tui/src/lib.rs`:

```rust
//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

pub mod persist;
```

Wire the new crate into `polaris-cli`. In `crates/polaris-cli/Cargo.toml`, add to `[dependencies]`:

```toml
polaris-tui = { path = "../polaris-tui" }
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p polaris-tui`
Expected: PASS — 3 tests.

- [ ] **Step 6: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add Cargo.toml crates/polaris-tui crates/polaris-cli/Cargo.toml docs/filemap.md
git commit -m "feat: add polaris-tui crate with session persistence"
```

---

### Task 3: Render module

**Files:**
- Create: `crates/polaris-tui/src/render.rs`
- Modify: `crates/polaris-tui/src/lib.rs` (add `pub mod render;`)

**Interfaces:**
- Consumes: `polaris_core::session::Session`, `polaris_provider::{Message, Role}`.
- Produces: `polaris_tui::render::Status` (`enum { Idle, Thinking, Error(String) }`, `pub`), `polaris_tui::render::render_chat(frame: &mut ratatui::Frame, session: &Session, input: &str, status: &Status)`, `polaris_tui::render::render_approval_modal(frame: &mut ratatui::Frame, reason: &str)`. Task 5's `run()` calls both against a real `Terminal`; Task 4's `TuiApprover` calls `render_approval_modal` against the same `Terminal` handle it's given.

- [ ] **Step 1: Write the failing rendering tests**

Create `crates/polaris-tui/src/render.rs`:

```rust
//! Pure rendering: turns a `Session` + input state into terminal cells.
//! Kept free of any actual terminal I/O so it's testable with
//! `ratatui::backend::TestBackend`.

use polaris_core::session::Session;
use polaris_provider::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

/// What the bottom-of-screen status line shows while a turn is in flight,
/// idle, or after a failed turn.
pub enum Status {
    Idle,
    Thinking,
    Error(String),
}

fn label(role: Role) -> &'static str {
    match role {
        Role::User => "you",
        Role::Assistant => "polaris",
        Role::Tool => "tool",
    }
}

fn history_lines(session: &Session) -> Vec<Line<'static>> {
    session
        .messages
        .iter()
        .filter(|m: &&Message| !matches!(m.role, Role::Tool))
        .map(|m| Line::from(format!("{}: {}", label(m.role), m.content)))
        .collect()
}

pub fn render_chat(frame: &mut Frame, session: &Session, input: &str, status: &Status) {
    let area = frame.area();
    let [history_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(area);

    let lines = history_lines(session);
    // Pin the view to the newest messages: once there are more lines than
    // fit, scroll so the last line lands on the last visible row. Without
    // this, a Paragraph always renders from line 0 and the most recent
    // reply — the thing a chat screen exists to show — scrolls off the
    // bottom out of view as the conversation grows.
    let visible_rows = history_area.height as usize;
    let scroll_offset = lines.len().saturating_sub(visible_rows) as u16;

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::TOP))
            .scroll((scroll_offset, 0)),
        history_area,
    );

    let status_text = match status {
        Status::Idle => String::new(),
        Status::Thinking => "thinking...".to_string(),
        Status::Error(e) => format!("error: {e}"),
    };
    frame.render_widget(Paragraph::new(status_text), status_area);

    frame.render_widget(
        Paragraph::new(input).block(Block::default().borders(Borders::ALL).title("input")),
        input_area,
    );
}

pub fn render_approval_modal(frame: &mut Frame, reason: &str) {
    let area = frame.area();
    let text = format!("Approval required: {reason}\n\n[y] allow   [n] deny");
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("approve?")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::session::Session;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn a_user_message_appears_with_its_role_label() {
        let mut session = Session::default();
        session.push_user("Cargo.toml は何行か");

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("you"));
        assert!(content.contains("Cargo.toml"));
    }

    #[test]
    fn the_status_line_shows_thinking_while_a_turn_is_in_flight() {
        let session = Session::default();
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Thinking))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("thinking"));
    }

    #[test]
    fn history_longer_than_the_screen_scrolls_to_show_the_newest_message() {
        let mut session = Session::default();
        for i in 0..30 {
            session.push_user(&format!("message number {i}"));
        }

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("message number 29"));
        assert!(!content.contains("message number 0 "));
    }

    #[test]
    fn the_approval_modal_shows_the_reason() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_approval_modal(f, "writing to src/main.rs"))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("writing to src/main.rs"));
    }
}
```

Note: push `session.push_user(...)` requires `mut session` — the test above already declares `let mut session`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `render` module not wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module in**

In `crates/polaris-tui/src/lib.rs`:

```rust
pub mod persist;
pub mod render;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p polaris-tui`
Expected: PASS — 7 tests total (3 from Task 2, 4 new).

If `Layout::areas` or `TestBackend::buffer()` don't match the installed `ratatui` version's API, check `cargo doc -p ratatui --open` or `~/.cargo/registry/src/*/ratatui-*/src/layout/mod.rs` for the resolved version's actual method names and adjust — the shapes above are correct for ratatui 0.29 but the workspace pin in Task 2 Step 1 may resolve to a newer patch release with a renamed method.

- [ ] **Step 5: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/render.rs crates/polaris-tui/src/lib.rs docs/filemap.md
git commit -m "feat: add polaris-tui render module"
```

---

### Task 4: `TuiApprover`

**Files:**
- Create: `crates/polaris-tui/src/approver.rs`
- Modify: `crates/polaris-tui/src/lib.rs` (add `pub mod approver;`)
- Modify: `crates/polaris-tui/Cargo.toml` (dev-dependency, if any is needed — none is; `KeyReader` is mocked with a plain struct, no new crate)

**Interfaces:**
- Consumes: `polaris_core::approval::{Approver, Decision}` (`trait Approver { fn ask(&mut self, reason: &str) -> Decision; }`), `render::render_approval_modal`.
- Produces: `polaris_tui::approver::KeyReader` (`pub trait { fn read_key(&mut self) -> std::io::Result<ratatui::crossterm::event::KeyCode>; }`), `polaris_tui::approver::CrosstermKeyReader` (blocking, real terminal), `polaris_tui::approver::TuiApprover<'a, B: ratatui::backend::Backend, R: KeyReader>` (`pub struct`, implements `Approver`). Task 5's `run()` constructs one `TuiApprover` per turn, borrowing the same `Terminal` it renders the chat screen with.

- [ ] **Step 1: Write the failing approver test**

Create `crates/polaris-tui/src/approver.rs`:

```rust
//! The `Approver` that runs inside the TUI: draws a modal over the current
//! frame and blocks on a single keypress. Generic over `KeyReader` so
//! tests can feed keys without a real terminal.

use polaris_core::approval::{Approver, Decision};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::KeyCode;

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
            if let ratatui::crossterm::event::Event::Key(k) = ratatui::crossterm::event::read()? {
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
```

This requires `Decision` to implement `PartialEq`/`Debug` for the `assert_eq!` calls — check `crates/polaris-core/src/approval.rs`: `Decision` already derives `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`, so no change needed there.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `approver` module not wired into `lib.rs` yet, and `KeyCode`'s `ratatui::crossterm` re-export needs the crossterm backend feature, which is `ratatui`'s default feature (already on since Task 2 added `ratatui` with no `default-features = false`).

- [ ] **Step 3: Wire the module in**

In `crates/polaris-tui/src/lib.rs`:

```rust
pub mod approver;
pub mod persist;
pub mod render;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p polaris-tui`
Expected: PASS — 10 tests total.

- [ ] **Step 5: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/approver.rs crates/polaris-tui/src/lib.rs docs/filemap.md
git commit -m "feat: add TuiApprover"
```

---

### Task 5: Input handling, the `run()` loop, and CLI wiring

**Files:**
- Create: `crates/polaris-tui/src/input.rs`
- Modify: `crates/polaris-tui/src/lib.rs` (add `pub mod input;`, add `RunArgs` and `run`)
- Modify: `crates/polaris-cli/src/main.rs` (branch into `polaris_tui::run` when `--prompt` is omitted)

**Interfaces:**
- Consumes: everything produced by Tasks 1-4, plus `polaris_core::{agent, audit::AuditLog, approval::Gate, stop::StopTracker, agent::ToolContext}`, `polaris_provider::Provider`, `polaris_sandbox::SandboxPolicy`.
- Produces: `polaris_tui::input::{InputAction, apply_key}` (pure, `pub`), `polaris_tui::RunArgs<'a>` (`pub struct`, field list below), `polaris_tui::run(args: RunArgs) -> std::process::ExitCode` (`pub`). `polaris-cli::main()` is the only caller.

`polaris-tui` does not depend on `polaris-sandbox` or `polaris-auth` directly for provider/sandbox *construction* — those are built once in `polaris-cli::main()` exactly as they are today and handed in already-constructed. `polaris-tui` does need the `polaris_sandbox::SandboxPolicy` *type* (to store it and build a `ToolContext`), so it does need `polaris-sandbox` as a dependency for that type, but never constructs one itself.

- [ ] **Step 1: Write the failing input-handling tests**

Create `crates/polaris-tui/src/input.rs`:

```rust
//! Pure keystroke-to-action mapping for the input box. Kept separate from
//! `run()` so the editing rules (what Enter/Backspace/Ctrl-C do) are
//! testable without a real terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum InputAction {
    Continue,
    Submit(String),
    Quit,
}

/// Applies one key event to the input buffer, returning what the caller
/// should do next. Mutates `buffer` in place for `Char`/`Backspace`.
pub fn apply_key(buffer: &mut String, key: KeyEvent) -> InputAction {
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
    use ratatui::crossterm::event::KeyEventKind;

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

    // Silence an unused-import warning if KeyEventKind isn't otherwise
    // referenced by this ratatui version's KeyEvent::new.
    #[allow(dead_code)]
    fn _unused(_: KeyEventKind) {}
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `input` module not wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module in and implement `run()`**

In `crates/polaris-tui/src/lib.rs`, replace the file with:

```rust
//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

pub mod approver;
pub mod input;
pub mod persist;
pub mod render;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use polaris_core::agent::{self, ToolContext};
use polaris_core::approval::{ApprovalPolicy, Gate};
use polaris_core::audit::AuditLog;
use polaris_core::prompt::AlwaysOn;
use polaris_core::session::Session;
use polaris_core::stop::StopTracker;
use polaris_provider::Provider;
use polaris_sandbox::SandboxPolicy;
use polaris_skills::Skill;

use approver::{CrosstermKeyReader, TuiApprover};
use input::{InputAction, apply_key};
use render::{Status, render_chat};

/// Everything `run()` needs, already built by `polaris-cli::main()` the
/// same way the one-shot path builds it. `polaris-tui` never constructs a
/// provider or a sandbox policy itself.
pub struct RunArgs<'a> {
    pub provider: &'a dyn Provider,
    pub state_dir: PathBuf,
    pub audit_path: PathBuf,
    pub max_turns: u32,
    pub sandbox: SandboxPolicy,
    pub helper: PathBuf,
    pub approval_policy: ApprovalPolicy,
    pub always_on: &'a AlwaysOn,
    pub skills: &'a [Skill],
}

pub async fn run(args: RunArgs<'_>) -> ExitCode {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        eprintln!("polaris: refusing to start the TUI on a non-interactive terminal");
        return ExitCode::FAILURE;
    }

    let session_path = args.state_dir.join("tui-session.jsonl");
    let (mut session, truncated) = match persist::load_session(&session_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Can't read {}: {e}", session_path.display());
            return ExitCode::FAILURE;
        }
    };

    let mut audit = match AuditLog::open(&args.audit_path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Can't open the audit log: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut terminal = ratatui::init();
    if truncated {
        eprintln!(
            "warning: {} had a corrupt line; resumed from the messages before it",
            session_path.display()
        );
    }

    let mut input_buffer = String::new();
    let mut status = Status::Idle;
    let mut key_reader = CrosstermKeyReader;

    let exit_code = 'outer: loop {
        if terminal
            .draw(|f| render_chat(f, &session, &input_buffer, &status))
            .is_err()
        {
            break ExitCode::FAILURE;
        }

        let event = match ratatui::crossterm::event::read() {
            Ok(e) => e,
            Err(_) => break ExitCode::FAILURE,
        };
        let ratatui::crossterm::event::Event::Key(key) = event else {
            continue;
        };

        let text = match apply_key(&mut input_buffer, key) {
            InputAction::Continue => continue,
            InputAction::Quit => break ExitCode::SUCCESS,
            InputAction::Submit(text) if text.trim().is_empty() => continue,
            InputAction::Submit(text) => text,
        };

        session.push_user(&text);
        if let Err(e) = persist::append_message(&session_path, session.messages.last().expect("just pushed")) {
            eprintln!("Can't persist the message: {e}");
            break 'outer ExitCode::FAILURE;
        }

        status = Status::Thinking;
        if terminal
            .draw(|f| render_chat(f, &session, &input_buffer, &status))
            .is_err()
        {
            break ExitCode::FAILURE;
        }

        let mut stop = StopTracker::new(args.max_turns);
        let mut gate = Gate::new(args.approval_policy);
        let mut approver = TuiApprover {
            terminal: &mut terminal,
            reader: &mut key_reader,
        };
        let mut ctx = ToolContext {
            sandbox: &args.sandbox,
            helper: &args.helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        match agent::run(
            args.provider,
            &mut session,
            &mut audit,
            &mut stop,
            args.always_on,
            args.skills,
            &mut ctx,
        )
        .await
        {
            Ok(_) => {
                status = Status::Idle;
                if let Some(reply) = session.messages.last() {
                    if let Err(e) = persist::append_message(&session_path, reply) {
                        eprintln!("Can't persist the reply: {e}");
                        break 'outer ExitCode::FAILURE;
                    }
                }
            }
            Err(e) => {
                status = Status::Error(e.to_string());
            }
        }
    };

    let _ = ratatui::restore();
    exit_code
}
```

Note on `agent::run`'s history side effect: it calls `session.push_assistant(...)` (or the tool-call variant) internally on success, so after a successful call `session.messages.last()` is the reply, already in `session` — this code only needs to additionally persist it to disk. On a tool-using turn, `agent::run` pushes multiple messages (assistant-with-tool-calls, tool results, final assistant reply) before returning; this task persists only the final one (`session.messages.last()`), which means intermediate tool-call/tool-result messages from a resumed session are not replayed to a freshly loaded `Session` after a restart. This is an accepted gap for this plan — call it out in Task 6's README notes so it's not silently lost; closing it (persisting every message `agent::run` pushes, not just the last) is a straightforward follow-up but out of scope here since `agent::run`'s signature doesn't expose per-push hooks today.

- [ ] **Step 4: Wire `polaris-cli::main()` into the TUI path**

In `crates/polaris-cli/Cargo.toml`, confirm `polaris-tui = { path = "../polaris-tui" }` is present (added in Task 2 Step 1) and add `polaris-skills` if not already a direct dependency (it already is, per the existing `[dependencies]` block).

In `crates/polaris-cli/src/main.rs`, the current flow errors out immediately when `args.prompt` is `None` (the `let Some(prompt) = args.prompt.clone() else { ... return ExitCode::FAILURE };` block). Move that check so the provider/sandbox/skills/always-on setup (everything between that point and the final `match agent::run(...)`) runs regardless of whether a prompt was given, then branch at the end:

```rust
    // (unchanged: provider selection, state_dir, audit_path, sandbox, helper,
    // approval_policy, constitution, config, discovered skills, always_on —
    // all built exactly as today, just without the early prompt-required
    // return before them)

    match args.prompt.clone() {
        Some(prompt) => {
            let mut session = Session::new();
            session.push_user(&prompt);

            let mut audit = match AuditLog::open(&audit_path) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("Can't open the audit log: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let mut stop = StopTracker::new(args.max_turns);
            let mut gate = Gate::new(approval_policy);
            let mut approver = TerminalApprover;
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            match agent::run(
                provider.as_ref(),
                &mut session,
                &mut audit,
                &mut stop,
                &always_on,
                &discovered.skills,
                &mut ctx,
            )
            .await
            {
                Ok(text) => {
                    println!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        None => {
            polaris_tui::run(polaris_tui::RunArgs {
                provider: provider.as_ref(),
                state_dir,
                audit_path,
                max_turns: args.max_turns,
                sandbox,
                helper,
                approval_policy,
                always_on: &always_on,
                skills: &discovered.skills,
            })
            .await
        }
    }
```

Add `use polaris_tui;` is unnecessary since it's referenced by full path (`polaris_tui::run`, `polaris_tui::RunArgs`) — no new `use` needed given the existing `use polaris_core::{...}` style already in the file.

- [ ] **Step 5: Run the full workspace build and test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass (14 new `polaris-tui` tests plus every existing test unchanged). `cargo test --workspace` runs in a non-interactive environment, so `polaris_tui::run`'s own loop is never exercised by the automated suite — only its unit-testable pieces (`persist`, `render`, `approver`, `input`) are. This is intentional; see Task 6.

- [ ] **Step 6: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/input.rs crates/polaris-tui/src/lib.rs crates/polaris-cli/src/main.rs crates/polaris-cli/Cargo.toml docs/filemap.md
git commit -m "feat: wire the interactive TUI into polaris"
```

---

### Task 6: Manual verification and docs

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing new — this task only documents and manually exercises what Tasks 1-5 built.

`run()`'s actual terminal loop has no automated test (Task 5 Step 5 explains why: it needs a real TTY). This task adds the same kind of "here's how to check it by hand" section the existing `polaris login` flow already has in the README (see the `### 保管先とパーミッションを手で確かめる` section), and does the hands-on check once.

- [ ] **Step 1: Add a manual-verification section to the README**

In `README.md`, after the `## ChatGPT のサブスクリプションで使う` section (before or after its existing subsections — place it as a new top-level `## TUI` section, right before `## ライセンス`):

```markdown
## TUI

`--prompt` を省略して `polaris` を実行すると、対話TUIに入る。

```
polaris
```

会話は `~/.polaris/state/<project-id>/tui-session.jsonl` に1メッセージ1行で保存され、
同じディレクトリで次回 `polaris` を実行すると再開する。`Ctrl-C` で終了する。

ツール実行の承認は画面内のモーダルで `y`/`n` により行う。挙動は `--approval` で制御でき、
一発実行(`polaris -p "..."`)と同じ意味を持つ。

### 手で確かめる

自動テストは実端末を必要とする部分(画面描画・キー入力そのもの)を確認できない。
`polaris` を実行し、以下を目視で確認する。

- 会話履歴と入力欄が表示され、文字入力・Backspace・Enterで送信できる
- 送信後 `thinking...` の表示を経て応答が履歴に追加される
- 書き込みを伴う指示(例:「test.txtというファイルを作って」)で承認モーダルが出て、
  `y`/`n` で応答できる
- `Ctrl-C` で終了し、`~/.polaris/state/<project-id>/tui-session.jsonl` が作られていること
- 同じディレクトリで再度 `polaris` を実行すると、直前の会話が履歴に表示されること
```

- [ ] **Step 2: Manually run the checks above**

Run: `cargo build --release && POLARIS_API_KEY=sk-... ./target/release/polaris` (or `POLARIS_PROVIDER=codex ./target/release/polaris` after `polaris login`)

Work through every bullet in the README section just added. Note any that fail — if any do, this task is not done; go back and fix the relevant Task 1-5 code before proceeding.

- [ ] **Step 3: Run the full workspace build and test suite one more time**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass.

- [ ] **Step 4: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add README.md docs/filemap.md
git commit -m "docs: add TUI usage and manual verification steps"
```
