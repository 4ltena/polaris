# polaris TUI Onboarding Screen Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `polaris` (interactive TUI, `--prompt` omitted) starts with no usable OpenAI credential, show a selection screen (ChatGPT sign-in vs. API key entry) instead of failing with `POLARIS_API_KEY is not set`, and let the user complete setup without restarting.

**Architecture:** A new `polaris_auth::api_key` module stores a plain OpenAI API key at `~/.polaris/api_key.json` (0600, same atomic-write pattern as `store.rs`) — completely independent of `Credentials`/`auth.json`, so existing users' files are untouched. `polaris-cli::main()`'s `openai` provider-resolution arm gains a 2-stage key lookup (`POLARIS_API_KEY` env var, then `api_key.json`); when both are absent AND the run is interactive TUI mode (`--prompt` omitted), it calls a new `polaris_tui::onboarding::run()` instead of failing immediately, then retries resolution. A one-shot run (`--prompt` given) is never affected — it keeps failing immediately, exactly as today, so scripted/CI usage is unchanged. `polaris-tui` gains a new dependency on `polaris-auth` for this one module only (a deliberate, documented exception to v0.3.0's "polaris-tui never touches auth" boundary — onboarding is inherently a rendering+auth operation woven together, unlike the chat loop which cleanly keeps them separate).

**Tech Stack:** Rust 1.96, edition 2024, existing `polaris-auth`/`polaris-tui`/`polaris-cli` crates. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-21-polaris-tui-onboarding-design.md`

## Global Constraints

- Rust 1.96.0, edition 2024.
- This is **not** a version bump — no `CHANGELOG.md` entry, no git tag, at the end of this plan.
- `~/.polaris/auth.json` / `polaris_auth::Credentials` must not change shape or gain fields. The new `~/.polaris/api_key.json` is a fully separate file.
- Onboarding triggers **only** for the `openai` provider-resolution path (the default when `POLARIS_PROVIDER` is unset), and **only** in TUI mode (`--prompt` omitted). The `codex` arm's existing lazy-failure behavior (a provider object is always built successfully; "not logged in" surfaces on the first actual request) is unchanged by this plan — deliberately out of scope, so as not to touch the already-tested `codex` resolution path or its one-shot error-message tests. An explicit `POLARIS_PROVIDER=codex` user who isn't logged in still only finds out on first use, exactly as today.
- One-shot mode (`--prompt` given) never enters onboarding, TTY or not — scripted/CI usage must keep failing immediately and identically to today.
- Onboarding's own non-interactive-terminal guard must exist (mirroring `polaris_tui::run()`'s existing one) so a TUI-mode-but-non-TTY environment (e.g. an automated test harness) fails cleanly rather than hanging.
- `crates/polaris-core/tests/filemap.rs` asserts `docs/filemap.md` matches the repository. Any task that adds/removes a `.rs` file must end with `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` before committing.

---

### Task 1: `polaris_auth::api_key` — plain API key storage

**Files:**
- Create: `crates/polaris-auth/src/api_key.rs`
- Modify: `crates/polaris-auth/src/lib.rs` (add `pub mod api_key;`)

**Interfaces:**
- Produces: `polaris_auth::api_key::default_path() -> Result<PathBuf, AuthError>` (`~/.polaris/api_key.json`), `polaris_auth::api_key::save_to(path: &Path, key: &str) -> Result<(), AuthError>`, `polaris_auth::api_key::load_from(path: &Path) -> Result<Option<String>, AuthError>`. Task 4 (`polaris-tui::onboarding`) calls `save_to`; Task 5 (`polaris-cli::main`) calls `default_path`/`load_from`.

- [ ] **Step 1: Write the failing tests**

Create `crates/polaris-auth/src/api_key.rs`:

```rust
//! Storage for a plain OpenAI API key at `~/.polaris/api_key.json`.
//! Completely independent of `Credentials`/`auth.json` — the codex OAuth
//! store is never read or written by this module, and vice versa.

use std::path::{Path, PathBuf};

use crate::AuthError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stored {
    key: String,
}

/// The default storage location. `~/.polaris/api_key.json`.
pub fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AuthError::Io(std::io::Error::other("HOME is not set")))?;
    Ok(Path::new(&home).join(".polaris").join("api_key.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_key_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        save_to(&p, "sk-example").expect("failed to save");
        let got = load_from(&p).expect("failed to read").expect("missing");
        assert_eq!(got, "sk-example");
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let got = load_from(&dir.path().join("nope.json")).expect("nonexistence is not a failure");
        assert!(got.is_none());
    }

    #[test]
    fn the_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        save_to(&p, "sk-example").expect("failed to save");
        let mode = std::fs::metadata(&p)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "permissions are not 0600: {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn overwriting_replaces_the_stored_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        save_to(&p, "sk-first").expect("failed to save");
        save_to(&p, "sk-second").expect("failed to save");
        let got = load_from(&p).expect("failed to read").expect("missing");
        assert_eq!(got, "sk-second");
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_absence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        std::fs::write(&p, b"{ not json").expect("failed to write");
        let err = load_from(&p).expect_err("a corrupt file should fail");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "corrupt file resulted in something other than Decode: {err:?}"
        );
    }
}
```

- [ ] **Step 2: Run to see the tests fail**

Run: `cargo test -p polaris-auth api_key`
Expected: FAIL to compile — `save_to`/`load_from` not defined yet, and `api_key` isn't wired into `lib.rs` yet.

- [ ] **Step 3: Implement `save_to` and `load_from`**

Add to `crates/polaris-auth/src/api_key.rs`, above the `#[cfg(test)]` line — this mirrors `store.rs`'s `save_to`/`load_from` exactly (atomic tmp+rename, 0600 via both open-mode and `set_permissions`, same reasoning as `store.rs`'s own comments):

```rust
/// Saves. Same atomic tmp+rename, 0600-both-ways pattern as
/// `store::save_to` — see that function's comments for why both the
/// open-time mode and the post-write `set_permissions` call are needed.
pub fn save_to(path: &Path, key: &str) -> Result<(), AuthError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(&Stored { key: key.to_string() })
        .map_err(|e| AuthError::Decode(format!("could not serialize the API key: {e}")))?;

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads. Nonexistence is not a failure. Corruption is.
pub fn load_from(path: &Path) -> Result<Option<String>, AuthError> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AuthError::Io(e)),
    };
    let stored: Stored = serde_json::from_slice(&body)
        .map_err(|e| AuthError::Decode(format!("could not parse {}: {e}", path.display())))?;
    Ok(Some(stored.key))
}
```

- [ ] **Step 4: Wire the module in**

In `crates/polaris-auth/src/lib.rs`, add alongside the existing `pub mod` lines (alphabetically):

```rust
pub mod api_key;
pub mod login;
pub mod pkce;
pub mod store;
pub mod token;
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p polaris-auth api_key`
Expected: PASS — 5 tests.

- [ ] **Step 6: Run the full polaris-auth test suite and commit**

Run: `cargo test -p polaris-auth`
Expected: all pass (existing `store.rs`/`login.rs`/etc. tests untouched and unaffected).

Regenerate the filemap: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-auth/src/api_key.rs crates/polaris-auth/src/lib.rs docs/filemap.md
git commit -m "feat(polaris-auth): add api_key module for plain OpenAI key storage"
```

---

### Task 2: `polaris_tui::onboarding` — pure rendering

**Files:**
- Create: `crates/polaris-tui/src/onboarding.rs`
- Modify: `crates/polaris-tui/src/lib.rs` (add `pub mod onboarding;`)

**Interfaces:**
- Produces: `polaris_tui::onboarding::Choice` (`pub enum { ChatGpt, ApiKey }`, `Clone`/`Copy`/`PartialEq`), `polaris_tui::onboarding::render_choice_screen(frame: &mut Frame, selected: Choice)`, `polaris_tui::onboarding::render_api_key_prompt(frame: &mut Frame, typed_len: usize)` (`typed_len` — the number of characters typed so far, rendered as that many mask characters; the real key is never passed to a render function, so it can never end up on screen or in a test buffer by accident). Task 4's `run()` calls both.

- [ ] **Step 1: Write the failing tests**

Create `crates/polaris-tui/src/onboarding.rs`:

```rust
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
```

- [ ] **Step 2: Run to see the tests fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `onboarding` module not wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module in**

In `crates/polaris-tui/src/lib.rs`, add alongside the existing `pub mod` lines (alphabetically):

```rust
pub mod approver;
pub mod input;
pub mod onboarding;
pub mod persist;
pub mod render;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p polaris-tui`
Expected: PASS — 3 new tests, plus every pre-existing test unchanged.

- [ ] **Step 5: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/onboarding.rs crates/polaris-tui/src/lib.rs docs/filemap.md
git commit -m "feat(polaris-tui): add onboarding screen rendering"
```

---

### Task 3: `polaris_tui::onboarding` — key handling

**Files:**
- Modify: `crates/polaris-tui/src/onboarding.rs`

**Interfaces:**
- Consumes: `ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers}` (same crate `polaris-tui` already depends on).
- Produces: `polaris_tui::onboarding::ChoiceAction` (`pub enum { Continue, Toggle, Submit, Quit }`), `polaris_tui::onboarding::apply_choice_key(key: KeyEvent) -> ChoiceAction`, `polaris_tui::onboarding::KeyEntryAction` (`pub enum { Continue, Submit(String), Quit }`), `polaris_tui::onboarding::apply_key_entry_key(buffer: &mut String, key: KeyEvent) -> KeyEntryAction`. Task 4's `run()` consumes both.

This mirrors `input.rs`'s `apply_key`/`InputAction` pattern from the chat loop — pure, terminal-I/O-free, testable without a real terminal. Follow `input.rs`'s existing `KeyEventKind::Press`-only filtering (added in the v0.3.0 fix wave) for both new functions, so onboarding doesn't double-register keys on the same platforms the chat loop already guards against.

- [ ] **Step 1: Read `input.rs` first**

Read `crates/polaris-tui/src/input.rs` in full — note its `KeyEventKind::Press` gate and its `Ctrl-C` handling pattern (`key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)`). Both new functions in this task follow the same two conventions.

- [ ] **Step 2: Write the failing tests**

Add to `onboarding.rs` (above the existing `#[cfg(test)] mod tests` block, outside it):

```rust
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
```

Add tests inside the existing `mod tests` block:

```rust
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
```

- [ ] **Step 3: Run to see the tests fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `apply_choice_key`/`apply_key_entry_key` not defined until this step's code above is added (if you added the implementation and tests together, run this after only the test additions to confirm RED first, per TDD; the code above already includes both — split your actual edit into "tests first" then "implementation" if you want a clean RED, or note in your report that this task's RED/GREEN was combined since the functions are short enough that writing them alongside their tests was more natural — either is fine, just be honest about which you did).

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p polaris-tui`
Expected: PASS — 7 new tests plus every pre-existing test (including Task 2's 3) unchanged.

- [ ] **Step 5: Regenerate the filemap and commit**

This task adds no new file, so filemap regen should be a no-op — run it anyway to confirm:

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/onboarding.rs docs/filemap.md
git commit -m "feat(polaris-tui): add onboarding key handling"
```

---

### Task 4: `polaris_tui::onboarding::run()` — the orchestration function

**Files:**
- Modify: `crates/polaris-tui/src/onboarding.rs`
- Modify: `crates/polaris-tui/Cargo.toml` (add `polaris-auth` dependency)

**Interfaces:**
- Consumes: `polaris_auth::{ISSUER, login, api_key, store}` (Task 1's `api_key` module, plus the existing `login`/`store` modules).
- Produces: `polaris_tui::onboarding::Outcome` (`pub enum { ApiKeySaved, CodexLoggedIn }`), `polaris_tui::onboarding::OnboardingError` (`pub enum { NonInteractive, Cancelled, Auth(String) }`, `impl std::fmt::Display`), `polaris_tui::onboarding::run(auth_store_path: &Path, api_key_path: &Path) -> Result<Outcome, OnboardingError>` (`pub async fn`). Task 5 (`polaris-cli::main`) is the caller.

No automated test can exercise the real OAuth browser flow or real API key submission end-to-end (same limitation v0.3.0/v0.4.0 already documented for the chat loop's own terminal-owning `run()`). This task's own tests are limited to what's mechanically checkable without a real terminal or network: the non-interactive guard.

- [ ] **Step 1: Add `polaris-auth` as a dependency**

In `crates/polaris-tui/Cargo.toml`, add to `[dependencies]` (alphabetically):

```toml
polaris-auth = { path = "../polaris-auth" }
```

- [ ] **Step 2: Write the failing non-interactive-guard test**

Add to `onboarding.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_non_interactive_terminal_is_refused_immediately() {
        // This test process's own stdin/stdout are not a real TTY under
        // `cargo test`, so `run()` must hit its own guard and return
        // NonInteractive rather than trying to draw anything or hang.
        let dir = tempfile::tempdir().expect("temp dir");
        let auth_path = dir.path().join("auth.json");
        let key_path = dir.path().join("api_key.json");

        let err = run(&auth_path, &key_path)
            .await
            .expect_err("a non-interactive terminal must be refused");
        assert!(matches!(err, OnboardingError::NonInteractive));
    }
```

This needs `tokio` as a dev-dependency for `#[tokio::test]`. Add to `crates/polaris-tui/Cargo.toml`'s `[dev-dependencies]`:

```toml
tokio = { workspace = true }
```

- [ ] **Step 3: Run to see the test fail**

Run: `cargo test -p polaris-tui a_non_interactive_terminal_is_refused_immediately`
Expected: FAIL to compile — `run`/`Outcome`/`OnboardingError` not defined yet.

- [ ] **Step 4: Implement `Outcome`, `OnboardingError`, and `run()`**

Add to `onboarding.rs`, above the `#[cfg(test)]` line:

```rust
use std::io::IsTerminal;
use std::path::Path;

/// Which credential became available once onboarding completes
/// successfully. `polaris-cli::main()` uses this to decide which
/// provider arm to retry.
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
                write!(f, "polaris: refusing to start onboarding on a non-interactive terminal")
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
        if terminal.draw(|f| render_choice_screen(f, selected)).is_err() {
            break 'outer Err(OnboardingError::Auth("failed to draw the terminal".to_string()));
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
                                    ratatui::widgets::Paragraph::new(format!("sign-in failed: {e}\n\npress any key to try again")),
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
                    if terminal.draw(|f| render_api_key_prompt(f, typed.len())).is_err() {
                        break 'outer Err(OnboardingError::Auth("failed to draw the terminal".to_string()));
                    }
                    let event = match ratatui::crossterm::event::read() {
                        Ok(e) => e,
                        Err(_) => break 'outer Err(OnboardingError::Auth("failed to read a key".to_string())),
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
```

Note: `polaris_auth::AuthError` implements `std::error::Error`/`Display` via `thiserror`, so `e.to_string()` above produces the same messages `polaris-cli`'s one-shot path already prints (e.g. for `login::run`'s failure modes).

- [ ] **Step 5: Run the test to see it pass**

Run: `cargo test -p polaris-tui a_non_interactive_terminal_is_refused_immediately`
Expected: PASS.

- [ ] **Step 6: Run the full workspace build and test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass.

- [ ] **Step 7: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/onboarding.rs crates/polaris-tui/Cargo.toml docs/filemap.md
git commit -m "feat(polaris-tui): add onboarding run() orchestration"
```

---

### Task 5: Wire onboarding into `polaris-cli::main()`

**Files:**
- Modify: `crates/polaris-cli/src/main.rs`

**Interfaces:**
- Consumes: `polaris_tui::onboarding::{run, Outcome, OnboardingError}` (Task 4), `polaris_auth::api_key::{default_path, load_from}` (Task 1).
- Produces: nothing new externally — this task only changes control flow inside `main()`.

- [ ] **Step 1: Read the current provider-resolution block in full**

Read `crates/polaris-cli/src/main.rs` lines ~248-298 (the `match provider_name.as_str() { "openai" => {...} "codex" => {...} other => {...} }` block) to see its exact current shape before editing — the sketch below assumes today's shape but you must verify against the live file.

- [ ] **Step 2: Wrap the match in a retry loop, and give the `openai` arm a 2-stage key lookup**

Change the `provider_name` binding from `let provider_name = ...` (immutable) to `let mut provider_name = ...`, and wrap the existing `match provider_name.as_str() { ... }` in a `loop { ... }`, changing every arm's `Box::new(p)` (the success path) to `break Box::new(p)`. In the `openai` arm, replace the key lookup:

```rust
// before
let key = match std::env::var("POLARIS_API_KEY") {
    Ok(k) => k,
    Err(_) => {
        eprintln!("POLARIS_API_KEY is not set");
        return ExitCode::FAILURE;
    }
};
```

with:

```rust
let key = std::env::var("POLARIS_API_KEY").ok().or_else(|| {
    polaris_auth::api_key::default_path()
        .ok()
        .and_then(|p| polaris_auth::api_key::load_from(&p).ok().flatten())
});
let key = match key {
    Some(k) => k,
    None if args.prompt.is_none() => {
        let auth_store_path = match polaris_auth::store::default_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Can't determine where to store credentials: {e}");
                return ExitCode::FAILURE;
            }
        };
        let api_key_path = match polaris_auth::api_key::default_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Can't determine where to store the API key: {e}");
                return ExitCode::FAILURE;
            }
        };
        match polaris_tui::onboarding::run(&auth_store_path, &api_key_path).await {
            Ok(polaris_tui::onboarding::Outcome::ApiKeySaved) => continue,
            Ok(polaris_tui::onboarding::Outcome::CodexLoggedIn) => {
                provider_name = "codex".to_string();
                continue;
            }
            Err(polaris_tui::onboarding::OnboardingError::Cancelled) => {
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }
    None => {
        eprintln!("POLARIS_API_KEY is not set");
        return ExitCode::FAILURE;
    }
};
```

The `continue` statements re-enter the wrapping `loop`, re-evaluating `provider_name.as_str()` — after `ApiKeySaved`, the loop re-enters the `"openai"` arm and this time `polaris_auth::api_key::load_from` finds the just-saved key; after `CodexLoggedIn`, `provider_name` is now `"codex"`, so the loop enters the `"codex"` arm instead (which is otherwise completely unchanged by this task — do not modify it).

The `other => { ... return ExitCode::FAILURE; }` arm is also unchanged — an unknown `POLARIS_PROVIDER` value is a configuration error, not a missing-credential situation, and stays out of onboarding's scope per this plan's Global Constraints.

- [ ] **Step 3: Verify the loop's braces/structure compile**

Run: `cargo build -p polaris-cli 2>&1 | head -60`

The most likely mechanical issue is matching the `loop { ... }` wrapper's closing brace and the `let provider: Box<dyn polaris_provider::Provider> = loop { ... };` binding correctly — read the compiler's errors carefully if this doesn't compile on the first try; do not guess-and-check blindly, reason about brace matching from the error's line numbers.

- [ ] **Step 4: Add an integration test for the 2-stage key lookup**

Add to `crates/polaris-cli/tests/subcommands.rs` (follow the existing tests' style in this file — `Command::new(bin())`, `.env(...)`/`.env_remove(...)`, temp `HOME`):

```rust
/// The api_key.json fallback: with no POLARIS_API_KEY env var but a
/// previously-saved key file, one-shot mode should succeed in building
/// the provider (it will still fail later when the fake key is rejected
/// by a real network call, but that's not what this test checks — it
/// only checks that key *resolution* used the file instead of failing at
/// "POLARIS_API_KEY is not set").
#[test]
fn a_saved_api_key_file_is_used_when_the_env_var_is_absent() {
    let home = tempfile::tempdir().expect("temp directory");
    let key_path = home.path().join(".polaris").join("api_key.json");
    std::fs::create_dir_all(key_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&key_path, r#"{"key":"sk-from-file"}"#).expect("write key file");

    let out = Command::new(bin())
        .args(["-p", "x"])
        .env("HOME", home.path())
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .env("POLARIS_BASE_URL", "http://127.0.0.1:1")
        .output()
        .expect("could not launch");

    // It must NOT fail with the "not set" message — it should get past
    // key resolution and fail later (e.g. a connection error to the
    // deliberately-unreachable base URL), proving the file was read.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("POLARIS_API_KEY is not set"),
        "did not use the saved key file: {stderr}"
    );
}
```

- [ ] **Step 5: Run the full CLI test suite**

Run: `cargo test -p polaris-cli`
Expected: all pass, including the new test and every pre-existing test in `cli.rs`/`subcommands.rs`/`confined_helper.rs` — pay particular attention to `the_normal_path_without_a_prompt_refuses_the_tui_on_a_non_interactive_terminal` (still expects the TUI's own guard message, since it sets a real-looking `POLARIS_API_KEY` so onboarding is never reached) and `the_default_provider_is_still_openai`/`a_logged_out_codex_run_names_the_login_command`/`an_unknown_provider_name_fails_fast` (all one-shot mode, so onboarding is never reached and their exact stderr expectations are unchanged).

- [ ] **Step 6: Run the full workspace build and test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass.

- [ ] **Step 7: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-cli/src/main.rs crates/polaris-cli/tests/subcommands.rs docs/filemap.md
git commit -m "feat: show an onboarding screen when the TUI starts with no OpenAI credential"
```

---

### Task 6: Manual verification and docs

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing new — this task documents and manually exercises what Tasks 1-5 built.

Same constraint as every prior TUI plan's final task: the actual interactive behavior (choosing an option, watching the browser open, typing a masked key) needs a real terminal and cannot be verified by an automated subagent.

- [ ] **Step 1: Add a short paragraph to the README's `## TUI` section**

In `README.md`, add near the top of the existing `## TUI` section (after the opening `--prompt` paragraph, before the provider-precondition paragraph — since this changes what that precondition paragraph says), in the same terse Japanese style as the rest of the section:

```markdown
プロバイダの資格情報が無い状態で対話TUIを起動すると、資格情報が無いままエラーで終了する代わりに、
ChatGPTでサインインするかOpenAI APIキーを入力するかを選ぶ画面が出る。APIキーは
`~/.polaris/api_key.json` に0600で保存され、次回以降は環境変数無しで使われる
(`~/.polaris/auth.json` とは別ファイルで、既存のcodexログイン情報には触れない)。
この画面は対話TUI起動時のみで、一発実行(`polaris -p "..."`)では従来通り即エラーになる。
```

Also update the existing sentence that currently says "プロバイダが未設定のまま `polaris` を実行すると、旧来の「--prompt is required」ではなくプロバイダのエラーで失敗する" — this is no longer accurate for the TUI path (it now shows the onboarding screen instead of failing), so either remove that sentence or narrow it to explicitly say it still applies to one-shot mode only. Read the current exact wording in the file before editing it.

- [ ] **Step 2: Extend the manual-verification checklist**

In the same section's `### 手で確かめる` list, add:

```markdown
- `POLARIS_API_KEY`/`~/.polaris/api_key.json`/`~/.polaris/auth.json` いずれも無い状態で `polaris` を実行すると、選択画面が出ること
- ↑/↓および1/2キーで選択が切り替わり、Enterで確定できること
- 「OpenAI APIキーを入力」を選ぶと入力欄が出て、入力した文字が伏字(`*`)で表示され、
  Enterで確定した後そのまま対話が始まること(再実行不要)。終了後 `~/.polaris/api_key.json` が
  作られていること
- 「ChatGPTでサインイン」を選ぶとブラウザが開き、認可完了後そのまま対話が始まること
  (`polaris login` と同じ経路であることの確認)
```

- [ ] **Step 3: Manually run the checks**

Run: `cargo build --release`, then in a fresh `HOME` (e.g. `HOME=$(mktemp -d) ./target/release/polaris`) with no `POLARIS_API_KEY` set, work through every new bullet above. If you're an automated agent without a real terminal/network, do the same non-interactive proxy check every prior plan's final task used: confirm the release binary builds, and confirm `./target/release/polaris < /dev/null` (no env vars set, TUI mode, non-interactive) exits promptly with a message rather than hanging — report the interactive walkthrough itself as deferred to a human, exactly as before. Do not fabricate having done it.

- [ ] **Step 4: Run the full workspace build and test suite one more time**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass.

- [ ] **Step 5: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add README.md docs/filemap.md
git commit -m "docs: document the onboarding screen and its manual verification steps"
```
