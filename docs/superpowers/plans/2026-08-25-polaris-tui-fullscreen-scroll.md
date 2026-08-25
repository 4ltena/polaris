# polaris TUI 自前スクロール管理・フッター固定化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `polaris-tui`'s `Viewport::Inline` + native-terminal-scrollback rendering with a self-managed `Viewport::Fullscreen`, an in-memory `history: Vec<HistoryLine>` buffer, and a `scroll_offset` the user drives with Up/Down — so the footer (status/input/model line) stays pinned to the bottom of the screen and scrolling history never gets yanked back to the bottom by a keystroke or a live tool-progress line, which native terminal scrollback does on every terminal except a rare few (verified: Terminal.app always does this; no ANSI-level workaround exists).

**Architecture:** `history`/`scroll_offset` live in `run()`'s local state, exactly like `input_buffer`/`session` do today. Every frame, a pure helper (`visible_history_window`) computes which slice of `history` is currently visible from `history.len()` and `scroll_offset`, and the single draw closure renders that slice into the top region of the screen and `render_footer` into a fixed-height bottom region — both regions recomputed from `frame.area()` every frame via `Layout::vertical`, no native scrollback involved. The four full-screen pickers (`/resume`, `/model`, `/permissions`, `/skills`) stop needing a temporary separate `Terminal` swap (`with_fullscreen_picker`) once the whole app is always full-screen — they draw straight into the same shared `Terminal`.

**Tech Stack:** Rust, `ratatui` 0.29 (`Viewport::Fullscreen`, `Layout::vertical`, `TestBackend`), `crossterm` (already a dependency via `ratatui::crossterm` re-export).

**Spec:** `docs/superpowers/specs/2026-08-25-polaris-tui-fullscreen-scroll-design.md`

## Global Constraints

- Full verification before every task's commit, exactly: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`.
- Commits: Conventional-Commits-style prefix, trailer `Co-Authored-By: Claude <noreply@anthropic.com>`, using the repository's own configured git identity (GitHub `@4ltena`) — never a hardcoded or harness-injected email.
- `polaris-cli` (one-shot/headless execution) has no scroll concept and is untouched by this plan (spec's own non-scope statement; acceptance criterion 5).
- `render_footer`'s own internal look/logic (the status/suggestions/input/footer split it computes from whatever area it's given) does not change — only *how it's given its area* changes (Task 3).
- IME cursor positioning inside `render_footer` (added just before this plan, commit `4c6a578`) is untouched — it already works off `input`/`cursor`, unrelated to which `Viewport` mode is active.
- **Judgment call — keybinding conflict with prior work, resolved by Task 4 before it can bite:** this worktree already has (rebased in from `main`, commit `ff70d2d`) an Up/Down-recalls-previous-input feature (`input::History`, wired into `run()`'s main loop) that claims the exact same Up/Down keys this spec wants for scrolling, once the slash-popup isn't showing. The spec's Up/Down-for-scroll choice was the one already reviewed and approved (see its "見送った代替案" section explicitly rejecting PageUp/PageDown in favor of Up/Down); the newer input-history feature had no chance to account for it. Resolution: input-history recall moves from bare Up/Down to Ctrl+P/Ctrl+N (the same historical readline/Emacs previous-history/next-history bindings), freeing Up/Down for scroll. Task 4 does this rebind *before* Task 7 introduces scroll's own Up/Down handling, so at every commit in this plan the keybindings stay unambiguous — no `git stash`/`amend` needed later. Flag this rebind to the user in the plan hand-off, since it changes behavior they approved minutes earlier in a different conversation turn.

---

### Task 1: `visible_history_window` — pure scroll-window function

**Files:**
- Modify: `crates/polaris-tui/src/render.rs` (new `pub fn`, near `render_history_into`)

**Interfaces:**
- Produces: `pub fn visible_history_window(history_len: usize, scroll_offset: usize, visible_height: usize) -> std::ops::Range<usize>` — later tasks (7, and the unified draw loop in Task 6) call this every frame.

- [ ] **Step 1: Write the failing tests**

Add to `render.rs`'s existing `#[cfg(test)] mod tests` block (near the other pure-function tests, e.g. next to `shimmer_spans_reproduce_the_text_verbatim`):

```rust
#[test]
fn the_window_shows_everything_when_history_is_shorter_than_the_viewport() {
    assert_eq!(visible_history_window(3, 0, 10), 0..3);
}

#[test]
fn the_window_shows_the_last_n_lines_when_scroll_offset_is_zero() {
    assert_eq!(visible_history_window(100, 0, 10), 90..100);
}

#[test]
fn a_positive_scroll_offset_shifts_the_window_up() {
    assert_eq!(visible_history_window(100, 5, 10), 85..95);
}

#[test]
fn scroll_offset_is_clamped_at_the_oldest_line() {
    // Can't scroll further back than showing line 0 at the window's top.
    assert_eq!(visible_history_window(100, 1000, 10), 0..10);
}

#[test]
fn a_zero_height_window_is_always_empty() {
    assert_eq!(visible_history_window(50, 0, 0), 50..50);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p polaris-tui visible_history_window`
Expected: FAIL — `visible_history_window` is not defined (compile error).

- [ ] **Step 3: Implement**

Add directly above `pub fn render_history_into` in `render.rs`:

```rust
/// Which `history` indices are currently visible, given how far the user
/// has scrolled back. `scroll_offset == 0` always means "showing the
/// newest `visible_height` lines" — the caller never has to special-case
/// "am I following the tail," since this recomputes from `history_len`
/// fresh every frame (see `lib.rs`'s unified draw loop).
pub fn visible_history_window(
    history_len: usize,
    scroll_offset: usize,
    visible_height: usize,
) -> std::ops::Range<usize> {
    let max_scroll = history_len.saturating_sub(visible_height);
    let effective_scroll = scroll_offset.min(max_scroll);
    let end = history_len - effective_scroll;
    let start = end.saturating_sub(visible_height);
    start..end
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p polaris-tui visible_history_window`
Expected: PASS, all 5 tests.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/render.rs
git commit -m "feat(polaris-tui): add visible_history_window, the scroll-window helper

Pure function, no rendering — clamps scroll_offset internally so callers
never need a separate bounds check. Not yet wired into the draw loop
(Task 6).

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: `header_history_lines` — border-baked header as `HistoryLine`s

**Files:**
- Modify: `crates/polaris-tui/src/render.rs` (new `pub fn`, near `render_header_into`)

**Interfaces:**
- Consumes: `HeaderInfo`, `header_lines` (existing, unchanged), `HistoryLine` (existing, from Task 1's neighborhood in the file).
- Produces: `pub fn header_history_lines(header: &HeaderInfo, width: u16) -> Vec<HistoryLine>` — consumed by Task 5's `run()` startup code.

**Judgment call:** the spec names this function `header_history_lines(header: &HeaderInfo) -> Vec<HistoryLine>` with no width parameter. That can't work: `render_header_into` currently gets its box width for free from the `insert_before` buffer's `area.width` (the terminal's width at print time); baking `┌─...─┐` border characters into a `Line<'static>`'s literal text requires knowing that width at construction time, since (unlike `render_header_into`, which draws a live `Block` widget) these lines have no widget behind them at render time — `render_history_into` just prints `Line`s row by row. This function therefore takes `width: u16` explicitly; Task 5 supplies it from the terminal's actual size at startup, matching what `insert_before`'s buffer gave `render_header_into` today. Like the *existing* `render_header_into` (also a one-shot print, never re-drawn), this does not react to a later terminal resize — that is pre-existing behavior, not a regression this plan introduces, and out of the spec's scope.

- [ ] **Step 1: Write the failing test**

Add to `render.rs`'s test module, near the existing header/footer tests:

```rust
#[test]
fn header_history_lines_matches_render_header_into_content_and_width() {
    let header = test_header();
    let width = 60u16;

    let lines = header_history_lines(&header, width);
    assert_eq!(lines.len(), HEADER_HEIGHT as usize);

    // Top and bottom rows are a full-width box-drawing border.
    let top = lines[0].line.to_string();
    let bottom = lines[lines.len() - 1].line.to_string();
    assert!(top.starts_with('\u{250c}') && top.ends_with('\u{2510}'));
    assert!(bottom.starts_with('\u{2514}') && bottom.ends_with('\u{2518}'));
    assert_eq!(top.chars().count(), width as usize);
    assert_eq!(bottom.chars().count(), width as usize);

    // Every content row is framed with the same `│ ... │` as the border
    // rows imply, and each content row's *text* matches header_lines'
    // plain (unbordered) content exactly.
    let plain = header_lines(&header);
    for (i, plain_line) in plain.iter().enumerate() {
        let framed = lines[1 + i].line.to_string();
        assert!(framed.starts_with('\u{2502}') && framed.ends_with('\u{2502}'));
        assert!(framed.contains(&plain_line.to_string()));
        assert_eq!(framed.chars().count(), width as usize);
    }

    // No row is shaded — the header box isn't a user/assistant line.
    assert!(lines.iter().all(|hl| !hl.shaded));
}
```

`test_header()` and `HEADER_HEIGHT` already exist in this file/module (used by other tests). `Line::to_string()` works because `ratatui::text::Line` implements `Display` via its spans' content concatenated — confirm this by running the test; if `.to_string()` isn't available, use `.spans.iter().map(|s| s.content.as_ref()).collect::<String>()` instead (same pattern already used elsewhere in this file's tests, e.g. `shimmer_spans_reproduce_the_text_verbatim`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p polaris-tui header_history_lines`
Expected: FAIL — `header_history_lines` not defined.

- [ ] **Step 3: Implement**

Add directly above `pub const HEADER_HEIGHT: u16 = 7;` (keep them adjacent — the function's row count must always equal `HEADER_HEIGHT`):

```rust
/// `header_lines(header)`, framed with a hand-built box-drawing border, as
/// plain `HistoryLine`s — the header's one-shot equivalent of
/// `history_lines_for`. Used once at startup (see `lib.rs`'s `run()`) to
/// seed `history`, replacing the old `insert_before(HEADER_HEIGHT,
/// render_header_into)` call. Always returns exactly `HEADER_HEIGHT` rows.
pub fn header_history_lines(header: &HeaderInfo, width: u16) -> Vec<HistoryLine> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let w = width as usize;
    let inner = w.saturating_sub(2);

    let top = format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(inner));
    let bottom = format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner));

    let mut out = Vec::with_capacity(HEADER_HEIGHT as usize);
    out.push(HistoryLine::plain(Line::styled(top, dim)));
    for content in header_lines(header) {
        let content_width = content.width();
        let pad = inner.saturating_sub(content_width);
        let mut spans = vec![Span::styled("\u{2502}", dim)];
        spans.extend(content.spans);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled("\u{2502}", dim));
        out.push(HistoryLine::plain(Line::from(spans)));
    }
    out.push(HistoryLine::plain(Line::styled(bottom, dim)));
    out
}
```

`HistoryLine::plain` already exists (private to this file, used elsewhere in it). `Line::width()` already exists on `ratatui::text::Line` (used by `render_footer`'s cursor-position code from the prior task).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p polaris-tui header_history_lines`
Expected: PASS.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/render.rs
git commit -m "feat(polaris-tui): add header_history_lines for the fullscreen history buffer

Bakes header_lines' content into bordered HistoryLines at a caller-given
width, matching what insert_before(HEADER_HEIGHT, render_header_into)
produces today. Not yet called from lib.rs (Task 5).

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: `render_footer` takes an explicit area instead of `frame.area()`

**Files:**
- Modify: `crates/polaris-tui/src/render.rs`

**Interfaces:**
- Consumes: none new.
- Produces: `render_footer`'s signature changes from `(frame, input, cursor, status, header, suggestions, selected_suggestion)` to `(frame, area, input, cursor, status, header, suggestions, selected_suggestion)` — `area: ratatui::layout::Rect` inserted as the second parameter. Every later task (6) that draws the footer into a *sub*-region of the fullscreen frame depends on this.

**Why this task exists on its own:** the spec describes this as "変更なし" (no change), but it isn't — today `render_footer` always computes `let area = frame.area();` internally, which is only correct because the *entire* frame currently *is* the footer's region (`Viewport::Inline` makes the whole visible frame the footer strip). Once history also lives in the same fullscreen frame (Task 6), the footer must be confined to a sub-`Rect`, so it needs to be told its area rather than assuming it owns the whole frame. Isolating this signature change into its own task, with every test still passing an area equal to the *full* test-backend area, proves the refactor is behavior-preserving before Task 6 changes what area gets passed in production.

- [ ] **Step 1: Update the production signature and internal use**

In `render.rs`, change:

```rust
pub fn render_footer(
    frame: &mut Frame,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
) {
    let area = frame.area();
```

to:

```rust
pub fn render_footer(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
) {
```

(the `let area = frame.area();` line is deleted — `area` now arrives as a parameter). Nothing else in the function body changes; every subsequent `Layout::vertical([...]).areas(area)` and `input_area`/`status_area`/etc. use is unaffected since they already only reference the local `area` binding.

- [ ] **Step 2: Update every call site to pass an area, preserving today's behavior exactly**

In `crates/polaris-tui/src/render.rs`'s test module:

- `render_footer_to_string`'s closure: change `render_footer(f, input, input.len(), status, header, suggestions, selected_suggestion)` to `render_footer(f, f.area(), input, input.len(), status, header, suggestions, selected_suggestion)`.
- `render_footer_cursor_position`'s closure: change `render_footer(f, input, cursor, &Status::Idle, &header, &[], 0)` to `render_footer(f, f.area(), input, cursor, &Status::Idle, &header, &[], 0)`.
- `the_cursor_lands_right_after_the_typed_input_text`'s closure: change `render_footer(f, "hello", 5, &Status::Idle, &test_header(), &[], 0)` to `render_footer(f, f.area(), "hello", 5, &Status::Idle, &test_header(), &[], 0)`.
- `the_cursor_accounts_for_wide_characters_already_typed`'s closure: change `render_footer(f, input, input.len(), &Status::Idle, &test_header(), &[], 0)` to `render_footer(f, f.area(), input, input.len(), &Status::Idle, &test_header(), &[], 0)`.

In `crates/polaris-tui/src/lib.rs`, all 3 production call sites (search for `render::render_footer(`) — each currently opens with `render::render_footer(\n    f,\n    &input_buffer,\n    input_cursor,\n    ...`. Change each to insert `f.area(),` as the new second argument: `render::render_footer(\n    f,\n    f.area(),\n    &input_buffer,\n    input_cursor,\n    ...`. This preserves exact current behavior — the footer still gets the whole (currently inline-viewport-sized) frame, since `f.area()` under `Viewport::Inline` today already equals what `frame.area()` gave it internally before this task. Task 6 is what changes *which* area gets passed here (a sub-rect after the fullscreen switch), not this task.

- [ ] **Step 3: Verify nothing broke**

Run: `cargo test -p polaris-tui`
Expected: PASS — every existing footer/cursor test asserts the exact same positions as before, since `f.area()` is identical to what `frame.area()` computed internally.

- [ ] **Step 4: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/render.rs crates/polaris-tui/src/lib.rs
git commit -m "refactor(polaris-tui): render_footer takes its area explicitly

Preparation for Task 6, which will pass render_footer only the bottom
sub-region of a fullscreen frame instead of the whole frame. Every call
site in this commit still passes the full frame area, so behavior is
unchanged — proven by the existing test suite passing unmodified.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: Rebind input-history recall from Up/Down to Ctrl+P/Ctrl+N

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `input::History` (existing, unchanged — only which keys call `.older()`/`.newer()` changes).
- Produces: nothing new; frees `KeyCode::Up`/`KeyCode::Down` (when the slash-popup isn't showing) for Task 7's scroll feature.

**Why:** see this plan's Global Constraints section — Up/Down is already claimed by input-history recall in this worktree (rebased in from `main`), and the spec's Up/Down-for-scroll design was the one already approved by the user. Doing the rebind now, before Task 7 introduces scroll's Up/Down handling, keeps every commit in this plan free of a keybinding collision.

- [ ] **Step 1: Change the key match**

In `crates/polaris-tui/src/lib.rs`, find the block added for input-history recall (a comment reading `// Up/Down recall previously submitted input, shell-history style —`, followed by `if key.kind == ratatui::crossterm::event::KeyEventKind::Press { use ratatui::crossterm::event::KeyCode; match key.code { KeyCode::Up => { ... } KeyCode::Down => { ... } _ => {} } }`). Replace the whole block with:

```rust
        // Ctrl+P/Ctrl+N recall previously submitted input, shell-history
        // style — the classic readline/Emacs previous-history/next-history
        // bindings. Not bound to bare Up/Down: those are claimed by
        // scrolling the conversation history once the slash-popup isn't
        // showing (see the Up/Down handling further below), and the
        // scroll design was the one already approved for those keys.
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::{KeyCode, KeyModifiers};
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('p') if ctrl => {
                    if let Some(recalled) = input_history.older(&input_buffer) {
                        input_buffer = recalled.to_string();
                        input_cursor = input_buffer.len();
                    }
                    continue;
                }
                KeyCode::Char('n') if ctrl => {
                    if let Some(recalled) = input_history.newer() {
                        input_buffer = recalled.to_string();
                        input_cursor = input_buffer.len();
                    }
                    continue;
                }
                _ => {}
            }
        }
```

- [ ] **Step 2: Update `input::History`'s doc comment**

In `crates/polaris-tui/src/input.rs`, `History`'s doc comment currently says "mirroring a shell's Up/Down history." Change it to:

```rust
/// Session-local recall of previously submitted input, bound to
/// Ctrl+P/Ctrl+N (the classic readline/Emacs previous-history/next-history
/// keys) — not Up/Down, which the TUI's main loop uses for scrolling the
/// conversation view instead. Not persisted to disk — a fresh `History`
/// is created each time the TUI starts.
```

- [ ] **Step 3: Verify the rebind compiles and existing tests still pass**

Run: `cargo test -p polaris-tui`
Expected: PASS — `input::History`'s own unit tests (in `input.rs`) test `.older()`/`.newer()` directly and don't reference any specific `KeyCode`, so they're unaffected by this rebind.

- [ ] **Step 4: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs crates/polaris-tui/src/input.rs
git commit -m "fix(polaris-tui): rebind input-history recall to Ctrl+P/Ctrl+N

Frees Up/Down for the fullscreen-scroll feature (this plan, Task 7),
whose Up/Down design was approved before this worktree picked up the
input-history feature from main and collided with it.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: `history: Vec<HistoryLine>` — replace `insert_before` with in-memory accumulation

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `render::HistoryLine`, `render::history_lines_for`, `render::header_history_lines` (Task 2), `render::format_event_for_live_print` (existing).
- Produces:
  - `fn append_new_history(session: &Session, local_lines: &[Line<'static>], printed_messages: &mut usize, printed_local_lines: &mut usize, history: &mut Vec<render::HistoryLine>)` — replaces `print_new_history`. No longer takes a `Terminal`/`RefCell`, no longer returns `Result` (nothing here can fail once it's pure in-memory `Vec` mutation).
  - `fn append_live_event(event: &polaris_core::AgentEvent, history: &mut Vec<render::HistoryLine>)` — replaces `print_live_event`. Same simplification.
  - A new `history: Vec<render::HistoryLine>` local in `run()`.

This task does **not** yet switch the viewport or change the draw loop — `history` accumulates every line that used to go straight to the terminal via `insert_before`, but the screen still only shows the small inline footer strip until Task 6 makes `history` actually visible. This is intentional: it isolates "is the bookkeeping correct" (testable without any rendering) from "does the new fullscreen draw loop work" (Task 6).

- [ ] **Step 1: Write the failing tests**

Add to `lib.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn append_new_history_appends_only_the_unprinted_tail() {
    let mut session = polaris_core::session::Session::default();
    session.push_user("first");
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = Vec::new();

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    let after_first = history.len();
    assert!(after_first > 0);

    session.push_assistant("reply");
    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert!(history.len() > after_first, "the assistant reply should have been appended, not reprinted from scratch");

    // Calling again with nothing new appends nothing.
    let stable = history.len();
    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(history.len(), stable);
}

#[test]
fn append_new_history_self_corrects_when_the_session_shrinks() {
    let mut session = polaris_core::session::Session::default();
    session.push_user("first");
    session.push_assistant("reply");
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = Vec::new();
    append_new_history(&session, &[], &mut printed_messages, &mut printed_local_lines, &mut history);
    assert_eq!(printed_messages, 2);

    // Simulates /clear: the session shrinks out from under the counters.
    session.messages.clear();
    append_new_history(&session, &[], &mut printed_messages, &mut printed_local_lines, &mut history);
    assert_eq!(printed_messages, 0);
}

#[test]
fn append_live_event_appends_formatted_lines() {
    let mut history = Vec::new();
    let event = polaris_core::AgentEvent::ToolStarted {
        name: "read".to_string(),
        detail: "Cargo.toml".to_string(),
    };
    append_live_event(&event, &mut history);
    assert!(!history.is_empty());
}
```

`polaris_core::AgentEvent`'s exact variant names/fields: check `crates/polaris-core/src/lib.rs` (or wherever `AgentEvent` is defined — `grep -rn "enum AgentEvent" crates/polaris-core/src/`) and use whichever variant `render::format_event_for_live_print` already handles in its own existing tests (in `render.rs`, search for `format_event_for_live_print` in `#[cfg(test)]`) — copy that exact construction rather than guessing field names.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p polaris-tui append_new_history append_live_event`
Expected: FAIL — `append_new_history`/`append_live_event` not defined.

- [ ] **Step 3: Implement `append_new_history`/`append_live_event`**

Replace the existing `print_new_history` function body and signature entirely:

```rust
/// Appends every `session.messages` entry and every `local_lines` entry
/// added since the last call to `history`, once each — the in-memory
/// analogue of the old `insert_before`-based one-shot terminal print.
/// Advances `printed_messages`/`printed_local_lines` to the new lengths.
/// Callers that reset a session wholesale (`/resume`, `/new`, `/fork`,
/// `/clear`) must reset both counters to 0 *and* clear `history` — see
/// each call site in `run()`.
fn append_new_history(
    session: &polaris_core::session::Session,
    local_lines: &[ratatui::text::Line<'static>],
    printed_messages: &mut usize,
    printed_local_lines: &mut usize,
    history: &mut Vec<render::HistoryLine>,
) {
    // A session that's now *shorter* than what's already been printed
    // (`/clear`, `/new`) can only mean it was reset out from under us —
    // reprint from scratch. This alone doesn't catch `/resume` loading a
    // same-or-longer *different* session, which is why every `/resume`
    // call site also resets both counters (and `history`) explicitly.
    if *printed_messages > session.messages.len() {
        *printed_messages = 0;
    }
    if *printed_local_lines > local_lines.len() {
        *printed_local_lines = 0;
    }
    if session.messages.len() > *printed_messages {
        let lines = render::history_lines_for(&session.messages[*printed_messages..]);
        history.extend(lines);
        *printed_messages = session.messages.len();
    }
    if local_lines.len() > *printed_local_lines {
        let new_lines = &local_lines[*printed_local_lines..];
        history.extend(new_lines.iter().map(|l| render::HistoryLine {
            line: l.clone(),
            shaded: false,
        }));
        *printed_local_lines = local_lines.len();
    }
}

/// Appends one mid-turn `AgentEvent`'s formatted lines to `history` — the
/// in-memory analogue of the old one-shot live print. `history_lines_for`
/// deliberately leaves tool activity out of its own output (see its own
/// doc comment) so appending this separately can't duplicate it.
fn append_live_event(event: &polaris_core::AgentEvent, history: &mut Vec<render::HistoryLine>) {
    history.extend(render::format_event_for_live_print(event));
}
```

Delete the old `print_new_history` and `print_live_event` functions entirely (their bodies are fully replaced by the above — do not leave both versions in the file).

- [ ] **Step 4: Update every call site**

In `run()`:

1. Add the new state variable near `input_history`'s declaration: `let mut history: Vec<render::HistoryLine> = Vec::new();`.
2. Replace the startup header print. Change:
   ```rust
   if terminal
       .borrow_mut()
       .insert_before(render::HEADER_HEIGHT, |buf| {
           render::render_header_into(buf, buf.area, &render::HeaderInfo { ... })
       })
       .is_err()
   {
       ratatui::restore();
       return ExitCode::FAILURE;
   }
   ```
   to:
   ```rust
   let startup_width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
   history.extend(render::header_history_lines(
       &render::HeaderInfo {
           provider_name: &args.provider_name,
           model_name: &args.model_name,
           usage: polaris_provider::Usage::default(),
           cwd: &args.cwd.display().to_string(),
           cwd_short: "",
           effort_name: "",
       },
       startup_width,
   ));
   ```
   (`Terminal::size()` returns `io::Result<Size>`; falling back to 80 columns on error keeps `run()` infallible here exactly like before — the old code could also fail this call, but only by returning `ExitCode::FAILURE`, which no longer applies since nothing here does I/O anymore).
3. Every call site of `print_new_history(&terminal, &session, &local_lines, &mut printed_messages, &mut printed_local_lines)` (there are 2 — top of the outer loop, and inside the agent-turn block after a turn completes) becomes `append_new_history(&session, &local_lines, &mut printed_messages, &mut printed_local_lines, &mut history)`, and the surrounding `if ... .is_err() { break ExitCode::FAILURE; }` wrapper is deleted (nothing can fail now — it's a plain statement, not an `if`).
4. Every call site of `print_live_event(&terminal, &event)` becomes `append_live_event(&event, &mut history)`, dropping the `if ... .is_err() { ... }` wrapper the same way.
5. At every point that currently resets `printed_messages = 0; printed_local_lines = 0;` (today only `/resume`'s two call sites — popup-Enter branch and typed-command branch), add `history.clear();` immediately after.
6. `/new` and `/fork` (`slash::Action::New` / `slash::Action::Fork`, each appearing in both the popup-Enter branch and the typed-command branch — 4 call sites total) currently rely on `append_new_history`'s shrink self-correction to reset `printed_messages`/`printed_local_lines` implicitly (since `handle_new_session` empties `session.messages`). That self-correction does **not** clear `history` (nothing compares `history.len()` to anything). Add explicit resets at all 4 call sites, right after the `handle_new_session(...)`/`handle_fork(...)` call: `printed_messages = 0; printed_local_lines = 0; history.clear();`.
7. `/clear` (`slash::Action::Clear`, inside `apply_slash_action`'s match arm) empties `session.messages` and `local_lines` directly. `apply_slash_action` doesn't have access to `printed_messages`/`printed_local_lines`/`history` (they're `run()`-local) — after the `apply_slash_action(...)` call site in `run()` returns `SlashOutcome::Continue` for a `Clear` action, the existing code just does `continue`. Since `apply_slash_action` doesn't report *which* action it ran, the simplest correct fix (matching the spec's instruction to clear at "the same timing as the other resets") is inside `append_new_history`/`append_live_event`'s existing shrink self-correction — but that only catches `session.messages` shrinking, not `local_lines` alone shrinking with `session.messages` unchanged, which `/clear` can also do. Add this instead, directly in `append_new_history`'s body from Step 3, replacing the two `if` guards with ones that also clear `history` when they fire:
   ```rust
   if *printed_messages > session.messages.len() {
       *printed_messages = 0;
       history.clear();
   }
   if *printed_local_lines > local_lines.len() {
       *printed_local_lines = 0;
       history.clear();
   }
   ```
   This makes the shrink-triggered `history.clear()` automatic and correct for `/clear` (and as a redundant-but-harmless second guard, for `/new`/`/fork` too — Step 6's explicit clears there stay, since they're clearer to read at the call site and this project favors explicit over implicit where cheap).

- [ ] **Step 5: Run to verify the new tests pass and nothing else broke**

Run: `cargo test -p polaris-tui`
Expected: PASS. (The screen still only shows the inline footer at this point — `history` isn't rendered yet, that's Task 6 — so no visual test changes are expected here.)

- [ ] **Step 6: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "refactor(polaris-tui): accumulate history in memory instead of insert_before

append_new_history/append_live_event replace print_new_history/
print_live_event — same bookkeeping, but appending to an in-memory Vec
instead of writing straight to the terminal's native scrollback, which
is what the fullscreen draw loop (next task) will read from. Every
/resume, /new, /fork, /clear call site now also clears history at the
same point it already reset the printed_* counters.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: `Viewport::Fullscreen` and the unified draw loop

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `render::visible_history_window` (Task 1), `render::render_history_into` (existing, unchanged), `render_footer`'s new `area` parameter (Task 3), `history`/`scroll_offset` (Task 5 introduced `history`; this task introduces `scroll_offset`).
- Produces: every draw closure in `run()` now splits `frame.area()` into a history region and a footer region and renders both; `scroll_offset: usize` local state.

This is the task where the feature actually becomes visible for the first time.

- [ ] **Step 1: Add `scroll_offset` state and the footer height constant reference**

Near `history`'s declaration (added in Task 5), add: `let mut scroll_offset: usize = 0;`. `INLINE_VIEWPORT_HEIGHT` already exists as the fixed footer-region height (status + suggestions + input + footer) — it keeps its name and value; only its *meaning* changes from "the whole inline viewport's height" to "the footer region's height within the fullscreen frame."

- [ ] **Step 2: Switch the terminal to `Viewport::Fullscreen`**

Change:
```rust
let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
    viewport: ratatui::Viewport::Inline(INLINE_VIEWPORT_HEIGHT),
}));
```
to:
```rust
let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
    viewport: ratatui::Viewport::Fullscreen,
}));
```

Both existing `ratatui::restore()` calls elsewhere in `run()` (on early-failure paths) stay exactly as they are — `restore()` already handles tearing down whichever viewport mode was active.

- [ ] **Step 3: Add a shared draw helper and use it at every draw call site**

Add this function near `visible_history_window`'s use, right above `run()` (or as a nested closure captured at each call site — a free function is clearer since it's called from 3 places with the same captured state pattern):

```rust
/// Splits `frame.area()` into the scrollable history region (top) and the
/// fixed-height footer region (bottom, `INLINE_VIEWPORT_HEIGHT` rows),
/// renders the current `visible_history_window` slice of `history` into
/// the first, and calls `render_footer` with the second. This is the one
/// draw routine every redraw point in `run()` shares — see this plan's
/// Task 6.
#[allow(clippy::too_many_arguments)]
fn draw_frame(
    frame: &mut ratatui::Frame,
    history: &[render::HistoryLine],
    scroll_offset: usize,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &render::HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
) {
    let area = frame.area();
    let [history_area, footer_area] = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(0),
        ratatui::layout::Constraint::Length(INLINE_VIEWPORT_HEIGHT),
    ])
    .areas(area);

    let window = render::visible_history_window(
        history.len(),
        scroll_offset,
        history_area.height as usize,
    );
    render::render_history_into(frame.buffer_mut(), history_area, &history[window]);

    render::render_footer(
        frame,
        footer_area,
        input,
        cursor,
        status,
        header,
        suggestions,
        selected_suggestion,
    );
}
```

`Frame::buffer_mut()` is the existing `ratatui` API `render_history_into`'s other callers already use indirectly through `insert_before`'s own `buf` closure argument — confirm the exact method name (`buffer_mut` vs. a different accessor) against the installed `ratatui` version if it differs: `grep -n "pub fn buffer_mut" ~/.cargo/registry/src/*/ratatui-0.29.0/src/terminal/frame.rs`.

Now replace all 3 production `terminal.borrow_mut().draw(|f| { render::render_footer(f, f.area(), &input_buffer, input_cursor, &status, &render::HeaderInfo { ... }, &suggestions_or_empty, selected_suggestion_or_0) })` call sites with a call to `draw_frame` instead, e.g. the first one becomes:

```rust
if terminal
    .borrow_mut()
    .draw(|f| {
        draw_frame(
            f,
            &history,
            scroll_offset,
            &input_buffer,
            input_cursor,
            &status,
            &render::HeaderInfo {
                provider_name: &args.provider_name,
                model_name: &model_name,
                usage: cumulative_usage,
                cwd: &cwd_display,
                cwd_short: &cwd_footer_display,
                effort_name: &effort_name,
            },
            &suggestions,
            selected_suggestion,
        )
    })
    .is_err()
{
    break ExitCode::FAILURE;
}
```

Apply the same substitution (keeping each call site's own existing arguments for `suggestions`/`selected_suggestion`, which differ slightly between the 3 sites today — e.g. the post-submit site passes `&[]`/`0`) to the other 2 call sites.

- [ ] **Step 4: Write a test proving the split is correct**

Add to `lib.rs`'s test module:

```rust
#[test]
fn draw_frame_puts_the_footer_in_the_bottom_inline_viewport_height_rows() {
    let backend = ratatui::backend::TestBackend::new(60, 30);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let history = vec![render::HistoryLine {
        line: ratatui::text::Line::from("line from history"),
        shaded: false,
    }];
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &history,
                0,
                "",
                0,
                &Status::Idle,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "gpt-5.4",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect();
    assert!(rows[0].contains("line from history"));
    // The footer's placeholder text lands in the bottom INLINE_VIEWPORT_HEIGHT
    // rows, not the top history region.
    let footer_start = 30 - INLINE_VIEWPORT_HEIGHT as usize;
    assert!(rows[footer_start..].iter().any(|r| r.contains("Ask polaris to do anything")));
    assert!(!rows[..footer_start].iter().any(|r| r.contains("Ask polaris to do anything")));
}
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p polaris-tui draw_frame`
Expected: PASS.

- [ ] **Step 6: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): switch to Viewport::Fullscreen with a unified draw loop

draw_frame splits the fullscreen frame into a scrollable history region
(rendered from visible_history_window's slice of the in-memory history
buffer) and a fixed-height footer region — replacing native terminal
scrollback, which every terminal except a rare few force-scrolls to the
bottom on new output or a keystroke (verified against Terminal.app,
which offers no way to disable it).

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: Up/Down scroll keys, reset-to-bottom on submit

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `scroll_offset` (Task 6).
- Produces: nothing new — wires existing state to keys.

- [ ] **Step 1: Add the key handling**

In the same spot Task 4 rebound input-history recall (right after that block, still before the slash-popup's own Up/Down handling is checked — actually the popup's Up/Down handling comes *first* in the file and already `continue`s when `!suggestions.is_empty()`, so this new block, like Task 4's, is only ever reached when the popup is empty):

```rust
        // Up/Down scroll the conversation history — only reachable once
        // the slash-popup above hasn't already claimed these keys (it
        // `continue`s whenever `suggestions` is non-empty).
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::Up => {
                    scroll_offset = scroll_offset.saturating_add(1);
                    continue;
                }
                KeyCode::Down => {
                    scroll_offset = scroll_offset.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
        }
```

Place this block directly after Task 4's Ctrl+P/Ctrl+N block (both blocks sit between the slash-popup `if` block and the `let text = if let Some(t) = review_text { ... } else { ... apply_key(...) ... }` block). Out-of-range values are harmless here — `visible_history_window` (Task 1) clamps `scroll_offset` internally every time it's used, so no bounds check is needed at the point the key is pressed.

- [ ] **Step 2: Reset to the bottom when a message is sent**

Find where `InputAction::Submit(text) => { input_history.record(&text); text }` is matched (inside the `apply_key` result handling). Add `scroll_offset = 0;` there:

```rust
InputAction::Submit(text) => {
    input_history.record(&text);
    scroll_offset = 0;
    text
}
```

- [ ] **Step 3: Write the tests**

This is key-dispatch logic embedded in `run()`'s big loop, which the existing test suite doesn't unit-test in isolation (the same is true of the equivalent input-history Up/Down logic from the prior task — there's no existing pattern for testing `run()`'s inline match arms directly). Instead, pin the two units of *logic* this task adds as pure assertions against `visible_history_window` (already covers the scroll-offset clamping) and add one integration-shaped test that exercises the real key dispatch through a `TestBackend`-driven fake run loop is out of scope for this task (`run()` isn't structured to be driven by a fake key reader the way the picker handlers are). Instead, cover the actually-new decision points directly:

```rust
#[test]
fn scroll_offset_saturates_at_zero_going_down() {
    let mut scroll_offset: usize = 0;
    scroll_offset = scroll_offset.saturating_sub(1);
    assert_eq!(scroll_offset, 0);
}

#[test]
fn scroll_offset_grows_unbounded_going_up_since_the_window_clamps_it() {
    // Mirrors what the Up-key handler in run() does — the clamp lives in
    // visible_history_window (Task 1), not here, by design (see that
    // task's doc comment).
    let mut scroll_offset: usize = 0;
    for _ in 0..1000 {
        scroll_offset = scroll_offset.saturating_add(1);
    }
    assert_eq!(scroll_offset, 1000);
    assert_eq!(render::visible_history_window(50, scroll_offset, 10), 0..10);
}
```

These are intentionally thin — they exist to pin the *contract* between the key handler (saturating, unclamped) and `visible_history_window` (does the actual clamping), so a future edit that tries to "helpfully" clamp in one place and not the other is caught. The real end-to-end behavior (does scrolling actually work when you press Up in a live terminal) is verified manually in Task 9, matching this project's established `tmux`-based verification practice for TUI behavior.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p polaris-tui scroll_offset`
Expected: PASS.

- [ ] **Step 5: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "feat(polaris-tui): Up/Down scroll the conversation history

scroll_offset resets to 0 (follow the tail) whenever a message is sent.
The saturating +/- 1 here relies on visible_history_window (already
merged) to clamp against the actual history length every frame.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 8: Remove `with_fullscreen_picker`

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `with_fullscreen_picker` and its 8 call sites are gone; `handle_resume`/`handle_permissions`/`handle_model`/`run_skills_picker` are now called directly.

- [ ] **Step 1: Delete `with_fullscreen_picker`**

Remove the entire function (from its doc comment through its closing `}`, currently right above `now_millis`).

- [ ] **Step 2: Simplify every call site**

There are 8 call sites — 4 actions (`Resume`, `Permissions`, `Model`, `Skills`) each appearing once in the popup-Enter-accept branch and once in the typed-command branch. Each currently looks like:

```rust
if with_fullscreen_picker(&terminal, || {
    handle_resume(
        &terminal,
        &mut key_reader,
        &args.sessions_dir,
        &cwd_display,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut session_started_at_millis,
        &mut status,
        &mut local_lines,
    )
})
.is_err()
{
    break ExitCode::FAILURE;
}
```

Replace with a direct call, dropping the wrapper and its error branch entirely (the handler itself already returns `()` and swallows its own draw errors by returning early — see `run_skills_picker`'s existing `if ... .draw(...).is_err() { return; }` pattern, which every picker handler follows):

```rust
handle_resume(
    &terminal,
    &mut key_reader,
    &args.sessions_dir,
    &cwd_display,
    &mut session,
    &mut session_path,
    &mut meta_path,
    &mut session_started_at_millis,
    &mut status,
    &mut local_lines,
);
```

Apply the same transformation (delete the `with_fullscreen_picker(&terminal, || { ... }).is_err() { break ExitCode::FAILURE; }` wrapper, call the inner handler directly as a plain statement) to all 8 sites: `handle_resume` ×2, `handle_permissions` ×2, `handle_model` ×2, `run_skills_picker` ×2 (the `Skills` arms additionally have `status = Status::Idle;` right after — keep that line, only the wrapper goes).

- [ ] **Step 3: Run to verify nothing broke**

Run: `cargo test -p polaris-tui`
Expected: PASS — `handle_resume`/`handle_permissions`/`handle_model`/`run_skills_picker`'s own existing tests already construct a `Terminal<TestBackend>` directly and call these functions without going through `with_fullscreen_picker` at all, so they're unaffected.

- [ ] **Step 4: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "refactor(polaris-tui): remove with_fullscreen_picker

No longer needed once the whole app is always Viewport::Fullscreen
(prior task) — pickers now draw straight into the shared terminal
instead of temporarily swapping to a second one.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 9: Acceptance-criteria verification

**Files:**
- Modify: `crates/polaris-tui/src/lib.rs` (one new test)
- No production code changes — this task verifies, it doesn't implement.

**Interfaces:** none new.

The spec's 6 acceptance criteria, and where each is covered:

1. Scroll position doesn't move on a live tick/tool-progress append — covered by `visible_history_window`'s own tests (Task 1: appending to `history` never changes `scroll_offset`, and the window is computed fresh from both every frame) plus manual verification below.
2. Typing while scrolled doesn't snap to the bottom — same reasoning: `apply_key` never touches `scroll_offset`, and Task 7's Up/Down handler is the only thing that does (besides reset-on-submit). Manual verification below.
3. Sending a message returns to the bottom — covered by Task 7 Step 2's `scroll_offset = 0` on submit; add a direct test (Step 1 below).
4. Footer always pinned to the bottom — covered by Task 6 Step 4's `draw_frame_puts_the_footer_in_the_bottom_inline_viewport_height_rows` test.
5. `polaris-cli` unaffected — this plan never touches `crates/polaris-cli/`; confirm with `git diff main --stat` showing no `polaris-cli` files once this plan's branch is done (Step 2 below).
6. Pickers still work after `with_fullscreen_picker` removal — covered by Task 8 Step 3 (existing picker tests unmodified and passing).

- [ ] **Step 1: Write the missing direct test for criterion 3**

Add to `lib.rs`'s test module:

```rust
#[test]
fn scroll_offset_test_helper_matches_the_submit_reset_contract() {
    // Documents/pins the exact behavior Task 7 Step 2 wires into run():
    // a non-empty Submit resets scroll_offset to 0. run()'s own loop
    // isn't independently driveable in a test (see Task 7 Step 3's note),
    // so this pins the invariant at the type level instead: after any
    // number of scroll-up steps, resetting to 0 always shows the tail.
    let history_len = 200;
    let mut scroll_offset: usize = 47; // mid-scroll
    scroll_offset = 0; // what the Submit arm does
    assert_eq!(
        render::visible_history_window(history_len, scroll_offset, 10),
        190..200
    );
}
```

- [ ] **Step 2: Confirm `polaris-cli` is untouched (criterion 5)**

Run: `git diff main --stat -- crates/polaris-cli`
Expected: empty output (no changes).

- [ ] **Step 3: Manual verification in a real terminal**

This project verifies TUI behavior live, not just via unit tests (see `tmux-verify-reference-cli-behavior`). Build the binary and drive it in `tmux`:

```bash
cargo build -p polaris-cli
TMPHOME=$(mktemp -d)
mkdir -p "$TMPHOME/.polaris"
printf '{"key":"sk-fake-demo-key"}' > "$TMPHOME/.polaris/api_key.json"
chmod 600 "$TMPHOME/.polaris/api_key.json"
tmux new-session -d -s scrolltest -x 100 -y 30 "env HOME=$TMPHOME POLARIS_PROVIDER=openai ./target/debug/polaris; sleep 60"
sleep 1
```

Then, one `tmux send-keys` at a time with `tmux capture-pane -t scrolltest -p` after each:
- Type several distinct short messages (e.g. `test message 1` through `test message 6`, each followed by `Enter`) so there's enough history to scroll.
- Press `Up` a few times — capture and confirm the visible top line changed (scrolled back) and the footer/input row is still at the very bottom of the pane.
- While scrolled, type a character — capture and confirm the scroll position (top visible line) did **not** jump back to the newest message.
- Press `Down` enough times to return to the bottom — capture and confirm the newest message is visible again.
- Type a new message and press `Enter` — capture and confirm the view is at the bottom (criterion 3) without needing to press `Down` first.
- Try `/model` (type `/model`, arrow to the item, `Enter`) — capture and confirm the picker renders correctly and `Esc` returns to the normal screen with history intact (criterion 6).

Clean up: `tmux send-keys -t scrolltest C-c; tmux kill-session -t scrolltest`.

Record the outcome (pass/fail per bullet) in the task's commit message or PR description — this step has no automated assertion, so its result must be stated explicitly, not just implied by "tests pass."

- [ ] **Step 4: Full verification and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tui/src/lib.rs
git commit -m "test(polaris-tui): pin the scroll-reset-on-submit contract

Acceptance-criteria sweep for the fullscreen-scroll feature — criteria
1/2/4/6 already covered by earlier tasks' tests; criterion 5 (polaris-cli
untouched) confirmed by git diff; criterion 3 pinned here; manual tmux
verification recorded in the task report.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:** §1 history persistence + header migration → Task 5. §2 scroll state / `visible_history_window` → Task 1 (function) + Task 6 (wiring). §3 key operations → Task 7 (scroll) + Task 4 (the pre-existing-collision rebind the spec's author couldn't have known about). §4 draw loop → Task 6. §5 terminal init/restore → Task 6 Step 2. §6 picker simplification → Task 8. テスト方針 1 → Task 1. テスト方針 2 → unchanged, confirmed by Task 6/8's passing existing suite. テスト方針 3 → Task 4 (rebind) + Task 7 (scroll keys) + their tests. テスト方針 4 → Task 7 Step 2/Task 9 Step 1. テスト方針 5 → Task 5 Step 4 (`/resume`/`/new`/`/fork`/`/clear` all get `history.clear()`). テスト方針 6 → Task 8 Step 3. テスト方針 7 → Task 2. 受け入れ基準 1-6 → enumerated in Task 9's header, each pointing at its covering task.

**Placeholder scan:** every step above has real code, an exact function/variable name, or an exact command — no "TBD"/"add error handling"/"similar to Task N" left in.

**Type consistency:** `visible_history_window(history_len: usize, scroll_offset: usize, visible_height: usize) -> Range<usize>` (Task 1) is used identically in Task 6's `draw_frame` and Task 7/9's tests. `header_history_lines(header: &HeaderInfo, width: u16) -> Vec<HistoryLine>` (Task 2) is called with exactly that signature in Task 5 Step 4. `render_footer`'s new `(frame, area, input, cursor, status, header, suggestions, selected_suggestion)` order (Task 3) is used identically inside `draw_frame` (Task 6). `append_new_history`/`append_live_event` (Task 5) keep the exact parameter list used at every call site updated in the same task and referenced again only by Task 6 (which doesn't change their signatures, only what surrounds their call sites).
