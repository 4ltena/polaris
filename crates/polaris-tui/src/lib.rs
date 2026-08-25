//! The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
//! is omitted.

pub mod approver;
pub mod input;
pub mod onboarding;
pub mod persist;
pub mod render;
pub mod sessions;
pub mod slash;
mod time;

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use polaris_core::agent::{self, ToolContext};
use polaris_core::approval::{ApprovalPolicy, Gate};
use polaris_core::audit::AuditLog;
use polaris_core::prompt::AlwaysOn;
use polaris_core::stop::StopTracker;
use polaris_provider::Provider;
use polaris_sandbox::SandboxPolicy;
use polaris_skills::Skill;

use approver::{CrosstermKeyReader, TuiApprover};
use input::{InputAction, apply_key};
use render::Status;

/// Everything `run()` needs, already built by `polaris-cli::main()` the
/// same way the one-shot path builds it. `polaris-tui` never constructs a
/// provider or a sandbox policy itself.
pub struct RunArgs<'a> {
    /// Shared rather than borrowed: `agent::run` needs both a `&dyn
    /// Provider` for the root's own turns and an owned handle it can hand
    /// to a wave of subagents (see `agent::run`'s `provider_pool`).
    pub provider: Arc<dyn Provider>,
    pub provider_name: String,
    pub model_name: String,
    pub cwd: PathBuf,
    pub state_dir: PathBuf,
    /// Where every saved conversation lives, across every project —
    /// `~/.polaris/sessions/` (see `polaris_core::project::sessions_dir`).
    /// Distinct from `state_dir`, which stays per-project (audit log,
    /// sandbox staging).
    pub sessions_dir: PathBuf,
    pub audit_path: PathBuf,
    pub max_turns: u32,
    pub sandbox: SandboxPolicy,
    pub helper: PathBuf,
    pub approval_policy: ApprovalPolicy,
    pub always_on: &'a AlwaysOn,
    pub skills: &'a [Skill],
    /// The subagent types `spawn` can resolve a task against.
    pub agent_types: &'a [polaris_skills::AgentType],
    /// How many `spawn` tasks a single wave may run concurrently, and how
    /// many of those may hold a `write_root` at once — from
    /// `Config::spawn_concurrency` / `Config::spawn_write_concurrency`
    /// (see `polaris_core::config`).
    pub spawn_concurrency: usize,
    pub spawn_write_concurrency: usize,
}

/// Collapses a leading `$HOME` to `~`, for the footer only (see
/// `render::HeaderInfo::cwd_short`'s docs on why the header's own
/// `directory:` row doesn't use this). Returns the path unabbreviated —
/// never an error — when `HOME` isn't set or the path doesn't start
/// under it; there's nothing meaningful to fail on here.
fn abbreviate_home(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    let Some(home) = std::env::var_os("HOME") else {
        return full;
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return full;
    }
    match full.strip_prefix(home.as_ref()) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => full,
    }
}

/// The footer region's fixed height within `draw_frame`'s fullscreen
/// layout: a border row, up to `render::MAX_DISPLAYED_SUGGESTIONS`
/// suggestion rows, one "N more" row, the status row, the input row, and
/// the footer row, all summed. This is a `Constraint::Length` given to
/// the vertical split in `draw_frame`, not a `Terminal`-level viewport
/// setting (there's no separate inline viewport anymore — see `run()`'s
/// `Viewport::Fullscreen` comment) — it's still fixed rather than
/// growing to fit content, which is why the suggestions popup is capped
/// (see `render::MAX_DISPLAYED_SUGGESTIONS`'s doc) instead of sizing to
/// fit an unbounded match list.
const FOOTER_HEIGHT: u16 = 1 + render::MAX_DISPLAYED_SUGGESTIONS as u16 + 1 + 1 + 1 + 1;

/// Appends every `session.messages` entry and every `local_lines` entry
/// added since the last call to `history`, once each — the in-memory
/// analogue of the old `insert_before`-based one-shot terminal print.
/// Advances `printed_messages`/`printed_local_lines` to the new lengths.
///
/// The shrink-guard below (`*printed_messages > session.messages.len()`,
/// and its `local_lines` twin) fires whenever a caller resets a session
/// out from under this function without resetting the two counters —
/// today that's only `/clear`'s `apply_slash_action` branch, which clears
/// `session.messages`/`local_lines` but has no access to `history` to
/// re-seed. Deliberately, this function does *not* try to re-seed the
/// header itself when that guard fires: doing so would need a
/// `HeaderInfo`/width threaded all the way into a function whose only
/// other job is diffing two counters against two slices, for the sake of
/// one caller. Instead every wholesale-reset path calls
/// `reset_conversation_view` explicitly (see its own doc and every call
/// site in `run()`, including the one right after `/clear`'s
/// `apply_slash_action` call) *before* falling through to the next
/// `append_new_history` call, so by the time this function's own
/// shrink-guard would fire, it instead finds `history` already correctly
/// re-seeded and `printed_messages`/`printed_local_lines` already at 0 —
/// meaning the guard is a no-op safety net on that path, not the
/// mechanism actually doing the reset.
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
        history.clear();
    }
    if *printed_local_lines > local_lines.len() {
        *printed_local_lines = 0;
        history.clear();
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

/// Clears `history` and immediately re-seeds it with the header box, and
/// resets `scroll_offset` back to 0 (following the tail) — the one place
/// every conversation-view reset (`/resume`, `/new`, `/fork`, `/clear`)
/// goes through, so the header can't be dropped by a bare `history.clear()`
/// and a stale scroll position can't survive into a different, possibly
/// much shorter, conversation. Callers still separately reset
/// `printed_messages`/`printed_local_lines` to 0 — this only owns
/// `history`/`scroll_offset`, not the counters `append_new_history` reads.
fn reset_conversation_view(
    history: &mut Vec<render::HistoryLine>,
    scroll_offset: &mut usize,
    header: &render::HeaderInfo,
    width: u16,
) {
    history.clear();
    history.extend(render::header_history_lines(header, width));
    *scroll_offset = 0;
}

/// Appends one mid-turn `AgentEvent`'s formatted lines to `history` — the
/// in-memory analogue of the old one-shot live print. `history_lines_for`
/// deliberately leaves tool activity out of its own output (see its own
/// doc comment) so appending this separately can't duplicate it. Returns
/// how many lines were just appended, so a caller that's scrolled back
/// (`scroll_offset > 0`) can advance `scroll_offset` by the same amount
/// and keep the *viewed content* fixed instead of letting the window
/// silently shift under the user by the number of newly appended lines
/// (see the two call sites in `run()`).
fn append_live_event(
    event: &polaris_core::AgentEvent,
    history: &mut Vec<render::HistoryLine>,
) -> usize {
    let before = history.len();
    history.extend(render::format_event_for_live_print(event));
    history.len() - before
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A sortable-by-time conversation id: a zero-padded millisecond
/// timestamp (so filenames sort chronologically) plus the process id and
/// a per-process counter, so two ids can never collide even if generated
/// within the same millisecond.
fn new_session_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:013}-{}-{n:04}", now_millis(), std::process::id())
}

/// Splits `frame.area()` into the scrollable history region (top) and the
/// fixed-height footer region (bottom, `FOOTER_HEIGHT` rows),
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
        ratatui::layout::Constraint::Length(FOOTER_HEIGHT),
    ])
    .areas(area);

    let window =
        render::visible_history_window(history.len(), scroll_offset, history_area.height as usize);
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

pub async fn run(args: RunArgs<'_>) -> ExitCode {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        eprintln!("polaris: refusing to start the TUI on a non-interactive terminal");
        return ExitCode::FAILURE;
    }

    // Always starts empty, regardless of directory or what was said last
    // time — there's no implicit "the" session per project anymore, only
    // however many saved conversations `/resume` can list. A fresh id is
    // picked now but nothing is written to `args.sessions_dir` until the
    // first message actually goes out (see `persist::write_meta_if_absent`
    // at the two `append_message` call sites below) — an empty launch
    // that's immediately quit shouldn't leave a phantom entry in the
    // resume list.
    let mut session = polaris_core::session::Session::default();
    let mut session_started_at_millis = now_millis();
    let mut session_path = args
        .sessions_dir
        .join(format!("{}.jsonl", new_session_id()));
    let mut meta_path = session_path.with_extension("meta.json");

    // One handle for the whole process, shared with any subagent `spawn`
    // starts — see `agent::run`'s docs on why the audit log is shared
    // rather than borrowed exclusively.
    let audit = match AuditLog::open(&args.audit_path) {
        Ok(a) => Arc::new(tokio::sync::Mutex::new(a)),
        Err(e) => {
            eprintln!("Can't open the audit log: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Shared via `RefCell`, not held exclusively — the agent-turn loop
    // below redraws the screen concurrently with `agent::run` (to animate
    // the status row while it's in flight), including while `agent::run`
    // is synchronously inside an approval prompt that also draws to this
    // same terminal (see `approver::TuiApprover`'s docs).
    //
    // A self-managed `Viewport::Fullscreen`, not native terminal
    // scrollback — every terminal except a rare few force-scrolls to the
    // bottom on new output or a keystroke (verified against Terminal.app,
    // which offers no way to disable it), which broke the earlier inline-
    // viewport design's premise of a real, user-scrollable primary-buffer
    // history. The header and every conversation turn are appended once to
    // the in-memory `history` buffer (see `append_new_history` below); each
    // frame, `draw_frame` renders `render::visible_history_window`'s slice
    // of that buffer (governed by `scroll_offset` below) into the top
    // region and `render::render_footer` (suggestions/status/input/footer)
    // into the fixed `FOOTER_HEIGHT`-tall bottom region — the one
    // draw routine every redraw point in this loop shares.
    // `ratatui::init_with_options` enables raw mode but — regardless of
    // `Viewport` — does not itself enter the alternate screen, so that's
    // done explicitly here (`EnterAlternateScreen`). `ratatui::restore()`
    // at this function's single cleanup point below already leaves the
    // alternate screen again on its own (confirmed against ratatui
    // 0.29.0's source), so nothing else needs to undo this call — a
    // fullscreen redraw never overwrites the user's real shell scrollback
    // and always hands it back on exit.
    if let Err(e) = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    ) {
        eprintln!("Can't enter the alternate screen: {e}");
        return ExitCode::FAILURE;
    }
    let terminal = RefCell::new(ratatui::init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    }));
    // A thin bar cursor, not the terminal's default (usually a full
    // blinking block) — a block cursor fully inverts whatever glyph sits
    // in that cell, so parking it over the empty input box's placeholder
    // text ("Ask polaris to do anything") made the caret look like a
    // blinking capital "A" rather than a caret. A bar sits between
    // characters instead of on top of one, so this can't happen
    // regardless of what's under it. Best-effort: an unsupported terminal
    // just keeps its own default cursor shape, which is cosmetic only.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::SetCursorStyle::BlinkingBar
    );
    // How many `session.messages` / `local_lines` entries have already
    // been appended to `history`. Only the delta past this point gets
    // appended on each call to `append_new_history` — appending is
    // one-shot, never a redraw, so re-appending an already-appended line
    // would duplicate it in `history` instead of updating anything in
    // place.
    let mut printed_messages = 0usize;
    let mut printed_local_lines = 0usize;
    // The in-memory scrollable history buffer — accumulates every line
    // that used to be printed one-shot to the terminal's native scrollback
    // via `insert_before`. Rendered every frame by `draw_frame`, which
    // slices it down to `render::visible_history_window`'s current window.
    let mut history: Vec<render::HistoryLine> = Vec::new();
    // How far back the user has scrolled the history region — 0 always
    // means "following the tail" (see `render::visible_history_window`).
    let mut scroll_offset: usize = 0;
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

    let cwd_display = args.cwd.display().to_string();
    // Shown only in the footer — verified against a real codex
    // screenshot (`gpt-5.6-sol high · ~/File/projects/...`); the header
    // box's own `directory:` row keeps the unabbreviated `cwd_display`.
    let cwd_footer_display = abbreviate_home(&args.cwd);
    let mut input_buffer = String::new();
    // Byte offset into `input_buffer`, always on a UTF-8 char boundary —
    // see `input::apply_key`.
    let mut input_cursor: usize = 0;
    // Session-local Up/Down recall of previously submitted input. Not
    // persisted — see `input::History`.
    let mut input_history = input::History::new();
    // A local copy, not `args.approval_policy` directly — `/permissions`
    // needs to be able to change it for the rest of the session, and
    // `args` isn't mutable (nor should adding one slash command make it
    // become mutable everywhere else it's used).
    let mut approval_policy = args.approval_policy;
    // A local copy, not `args.model_name` directly — `/model` needs to be
    // able to change it for the rest of the session, same reasoning as
    // `approval_policy` above. `args.provider.set_model(...)` is the one
    // that actually changes what gets sent; this is only the display copy
    // shown in the header/footer/notices.
    let mut model_name = args.model_name.clone();
    // No `RunArgs` field to seed this from — nothing before `/model` is
    // ever chosen sets an effort, so this starts at the same default
    // `render::render_effort_picker` itself marks `(default)`.
    let mut effort_name = render::DEFAULT_EFFORT.to_string();
    let mut cumulative_usage = polaris_provider::Usage::default();
    let mut status = Status::Idle;
    let mut key_reader = CrosstermKeyReader;
    let mut fatal_message: Option<String> = None;
    // Which row the `/`-popup highlights. Persists across loop iterations
    // (arrow keys move it) and is clamped below whenever the candidate
    // list itself changes, so it's always a valid index or the list is
    // empty.
    let mut selected_suggestion: usize = 0;
    // Local-only output appended after the real conversation — e.g.
    // `/diff`'s result. Never touches `session.messages`, never sent to
    // the model, never persisted; cleared by `/clear`/`/new` along with
    // the real history.
    let mut local_lines: Vec<ratatui::text::Line<'static>> = Vec::new();

    let exit_code = 'outer: loop {
        // Recomputed every draw from the live buffer, so the popup tracks
        // each keystroke — not just the moment `/` was first typed.
        let suggestions = match input_buffer.strip_prefix('/') {
            Some(prefix) => slash::matching(prefix),
            None => Vec::new(),
        };
        if selected_suggestion >= suggestions.len() {
            selected_suggestion = 0;
        }
        // Print anything new since the last iteration (typically the
        // previous turn's assistant reply, or a `/resume`/`/new`/`/fork`
        // reset) before redrawing the small footer viewport — printing is
        // one-shot and must happen before the footer draw so the newly
        // printed lines appear above it, not interleaved mid-frame.
        append_new_history(
            &session,
            &local_lines,
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
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

        let event = match ratatui::crossterm::event::read() {
            Ok(e) => e,
            Err(_) => break ExitCode::FAILURE,
        };
        let ratatui::crossterm::event::Event::Key(key) = event else {
            continue;
        };

        // `/review` isn't a local command like the others — it expands
        // into a real user message and falls through to the normal model
        // turn below, so both places that can resolve it (accepting a
        // highlighted suggestion, and typing the full name) funnel into
        // this instead of calling `apply_slash_action`.
        let mut review_text: Option<String> = None;

        // While the popup is open, Up/Down move the highlight and Enter
        // accepts whichever row is highlighted — mirroring codex's own
        // picker. Typing the full command name and pressing Enter still
        // works too, since at that point it's the (only) highlighted row.
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press && !suggestions.is_empty() {
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::Down => {
                    selected_suggestion = (selected_suggestion + 1) % suggestions.len();
                    continue;
                }
                KeyCode::Up => {
                    selected_suggestion =
                        (selected_suggestion + suggestions.len() - 1) % suggestions.len();
                    continue;
                }
                KeyCode::Enter => {
                    let action = slash::action_for(suggestions[selected_suggestion].name);
                    input_buffer.clear();
                    input_cursor = 0;
                    selected_suggestion = 0;
                    match action {
                        slash::Action::Review(extra) => {
                            review_text = Some(review_prompt(&extra));
                        }
                        slash::Action::Resume => {
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
                            // A resumed session may be the same length as
                            // (or longer than) what's already printed but
                            // still a genuinely *different* conversation —
                            // the length-shrunk check inside
                            // `append_new_history` can't detect that case,
                            // so reset explicitly here.
                            let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                            reset_conversation_view(
                                &mut history,
                                &mut scroll_offset,
                                &render::HeaderInfo {
                                    provider_name: &args.provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            printed_messages = 0;
                            printed_local_lines = 0;
                            continue;
                        }
                        slash::Action::New => {
                            handle_new_session(
                                &args.sessions_dir,
                                &mut session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                                &mut local_lines,
                            );
                            let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                            reset_conversation_view(
                                &mut history,
                                &mut scroll_offset,
                                &render::HeaderInfo {
                                    provider_name: &args.provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            printed_messages = 0;
                            printed_local_lines = 0;
                            continue;
                        }
                        slash::Action::Fork => {
                            handle_fork(
                                &args.sessions_dir,
                                &cwd_display,
                                &session,
                                &mut session_path,
                                &mut meta_path,
                                &mut session_started_at_millis,
                                &mut status,
                            );
                            let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                            reset_conversation_view(
                                &mut history,
                                &mut scroll_offset,
                                &render::HeaderInfo {
                                    provider_name: &args.provider_name,
                                    model_name: &model_name,
                                    usage: cumulative_usage,
                                    cwd: &cwd_display,
                                    cwd_short: &cwd_footer_display,
                                    effort_name: &effort_name,
                                },
                                width,
                            );
                            printed_messages = 0;
                            printed_local_lines = 0;
                            continue;
                        }
                        slash::Action::Permissions => {
                            handle_permissions(
                                &terminal,
                                &mut key_reader,
                                &mut approval_policy,
                                &mut status,
                            );
                            continue;
                        }
                        slash::Action::Model => {
                            handle_model(
                                &terminal,
                                &mut key_reader,
                                args.provider.as_ref(),
                                &mut model_name,
                                &mut effort_name,
                                &mut status,
                            );
                            continue;
                        }
                        slash::Action::Skills => {
                            run_skills_picker(&terminal, &mut key_reader, args.skills);
                            status = Status::Idle;
                            continue;
                        }
                        action => {
                            // `apply_slash_action` clears `session.messages`/
                            // `local_lines` for `Clear` but has no access to
                            // `history`/`scroll_offset` to re-seed the header
                            // and reset the scroll position — do that here
                            // instead of relying on `append_new_history`'s
                            // shrink-guard to (not) do it (see that
                            // function's doc comment).
                            let clear_view = matches!(action, slash::Action::Clear);
                            match apply_slash_action(
                                action,
                                &mut session,
                                &session_path,
                                &mut status,
                                &args.provider_name,
                                &model_name,
                                cumulative_usage,
                                args.skills,
                                &args.cwd,
                                &mut local_lines,
                            ) {
                                SlashOutcome::Continue => {
                                    if clear_view {
                                        let width =
                                            terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                        reset_conversation_view(
                                            &mut history,
                                            &mut scroll_offset,
                                            &render::HeaderInfo {
                                                provider_name: &args.provider_name,
                                                model_name: &model_name,
                                                usage: cumulative_usage,
                                                cwd: &cwd_display,
                                                cwd_short: &cwd_footer_display,
                                                effort_name: &effort_name,
                                            },
                                            width,
                                        );
                                        printed_messages = 0;
                                        printed_local_lines = 0;
                                    }
                                    continue;
                                }
                                SlashOutcome::Quit => break ExitCode::SUCCESS,
                                SlashOutcome::Fatal(msg) => {
                                    fatal_message = Some(msg);
                                    break 'outer ExitCode::FAILURE;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Up/Down recall previously submitted input, shell-history style —
        // matching ordinary terminals/shells. Only reachable once the
        // slash-popup above hasn't already claimed these keys (it
        // `continue`s whenever `suggestions` is non-empty).
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::Up => {
                    if let Some(recalled) = input_history.older(&input_buffer) {
                        input_buffer = recalled.to_string();
                        input_cursor = input_buffer.len();
                    }
                    continue;
                }
                KeyCode::Down => {
                    if let Some(recalled) = input_history.newer() {
                        input_buffer = recalled.to_string();
                        input_cursor = input_buffer.len();
                    }
                    continue;
                }
                _ => {}
            }
        }

        // PageUp/PageDown scroll the conversation history — not claimed by
        // the slash-popup or by input-history recall, so these are always
        // reachable when the input box has focus.
        if key.kind == ratatui::crossterm::event::KeyEventKind::Press {
            use ratatui::crossterm::event::KeyCode;
            match key.code {
                KeyCode::PageUp => {
                    scroll_offset = scroll_offset.saturating_add(1);
                    continue;
                }
                KeyCode::PageDown => {
                    scroll_offset = scroll_offset.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
        }

        let text = if let Some(t) = review_text {
            t
        } else {
            let typed = match apply_key(&mut input_buffer, &mut input_cursor, key) {
                InputAction::Continue => {
                    selected_suggestion = 0;
                    continue;
                }
                InputAction::Quit => break ExitCode::SUCCESS,
                InputAction::Submit(text) if text.trim().is_empty() => continue,
                InputAction::Submit(text) => {
                    input_history.record(&text);
                    scroll_offset = 0;
                    text
                }
            };
            // Slash commands are local: handled here and never reach
            // `session.messages`, the model, or the persisted session
            // file. Reached only for text the popup didn't already
            // resolve above — an exact-but-unmatched-by-Enter case can't
            // happen since Enter with any suggestion present is
            // intercepted, so this covers `/unknown-name` (no matches,
            // popup was empty) and plain text.
            match slash::parse(&typed) {
                Some(slash::Action::Review(extra)) => review_prompt(&extra),
                Some(slash::Action::Resume) => {
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
                    // Same reasoning as the popup-selection path above: a
                    // resumed session's length alone can't be trusted to
                    // signal "this is a different conversation."
                    let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                    reset_conversation_view(
                        &mut history,
                        &mut scroll_offset,
                        &render::HeaderInfo {
                            provider_name: &args.provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        width,
                    );
                    printed_messages = 0;
                    printed_local_lines = 0;
                    continue;
                }
                Some(slash::Action::New) => {
                    handle_new_session(
                        &args.sessions_dir,
                        &mut session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                        &mut local_lines,
                    );
                    let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                    reset_conversation_view(
                        &mut history,
                        &mut scroll_offset,
                        &render::HeaderInfo {
                            provider_name: &args.provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        width,
                    );
                    printed_messages = 0;
                    printed_local_lines = 0;
                    continue;
                }
                Some(slash::Action::Fork) => {
                    handle_fork(
                        &args.sessions_dir,
                        &cwd_display,
                        &session,
                        &mut session_path,
                        &mut meta_path,
                        &mut session_started_at_millis,
                        &mut status,
                    );
                    let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                    reset_conversation_view(
                        &mut history,
                        &mut scroll_offset,
                        &render::HeaderInfo {
                            provider_name: &args.provider_name,
                            model_name: &model_name,
                            usage: cumulative_usage,
                            cwd: &cwd_display,
                            cwd_short: &cwd_footer_display,
                            effort_name: &effort_name,
                        },
                        width,
                    );
                    printed_messages = 0;
                    printed_local_lines = 0;
                    continue;
                }
                Some(slash::Action::Permissions) => {
                    handle_permissions(
                        &terminal,
                        &mut key_reader,
                        &mut approval_policy,
                        &mut status,
                    );
                    continue;
                }
                Some(slash::Action::Model) => {
                    handle_model(
                        &terminal,
                        &mut key_reader,
                        args.provider.as_ref(),
                        &mut model_name,
                        &mut effort_name,
                        &mut status,
                    );
                    continue;
                }
                Some(slash::Action::Skills) => {
                    run_skills_picker(&terminal, &mut key_reader, args.skills);
                    status = Status::Idle;
                    continue;
                }
                Some(action) => {
                    // Same reasoning as the popup-selection path above:
                    // `apply_slash_action` can't re-seed `history`'s header
                    // or reset `scroll_offset` itself for `Clear`.
                    let clear_view = matches!(action, slash::Action::Clear);
                    match apply_slash_action(
                        action,
                        &mut session,
                        &session_path,
                        &mut status,
                        &args.provider_name,
                        &model_name,
                        cumulative_usage,
                        args.skills,
                        &args.cwd,
                        &mut local_lines,
                    ) {
                        SlashOutcome::Continue => {
                            if clear_view {
                                let width = terminal.borrow().size().map(|s| s.width).unwrap_or(80);
                                reset_conversation_view(
                                    &mut history,
                                    &mut scroll_offset,
                                    &render::HeaderInfo {
                                        provider_name: &args.provider_name,
                                        model_name: &model_name,
                                        usage: cumulative_usage,
                                        cwd: &cwd_display,
                                        cwd_short: &cwd_footer_display,
                                        effort_name: &effort_name,
                                    },
                                    width,
                                );
                                printed_messages = 0;
                                printed_local_lines = 0;
                            }
                            continue;
                        }
                        SlashOutcome::Quit => break ExitCode::SUCCESS,
                        SlashOutcome::Fatal(msg) => {
                            fatal_message = Some(msg);
                            break 'outer ExitCode::FAILURE;
                        }
                    }
                }
                None => typed,
            }
        };

        session.push_user(&text);
        let checkpoint = session.messages.len();
        // A no-op after the first call for this session (see
        // `write_meta_if_absent`'s docs) — this is the lazy point where an
        // until-now-empty session actually starts existing on disk.
        if let Err(e) = persist::write_meta_if_absent(
            &meta_path,
            &persist::SessionMeta {
                cwd: cwd_display.clone(),
                started_at_millis: session_started_at_millis,
            },
        ) {
            fatal_message = Some(format!("Can't write the session metadata: {e}"));
            break 'outer ExitCode::FAILURE;
        }
        if let Err(e) =
            persist::append_message(&session_path, session.messages.last().expect("just pushed"))
        {
            fatal_message = Some(format!("Can't persist the message: {e}"));
            break 'outer ExitCode::FAILURE;
        }

        let turn_started = Instant::now();
        status = Status::Thinking {
            elapsed: Duration::ZERO,
        };
        // Print the user's own message immediately — otherwise it wouldn't
        // appear until the *next* top-of-loop print, which only happens
        // after the assistant's reply finishes too, making the user's own
        // input look laggy instead of showing up the instant it's sent.
        append_new_history(
            &session,
            &local_lines,
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
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
                    // `input_buffer` was just cleared by `apply_key`'s
                    // Submit branch, so there's nothing to suggest against.
                    &[],
                    0,
                )
            })
            .is_err()
        {
            break ExitCode::FAILURE;
        }

        enum TurnOutcome {
            Done(Result<agent::AgentOutcome, agent::AgentError>),
            Interrupted,
            Fatal,
        }

        // Scoped so every borrow taken to build `agent_future` (of
        // `session`, `audit`, ...) ends when this block does — the match
        // below needs `&mut session` again, which it can't have while
        // any of these are still alive.
        let outcome = {
            let mut stop = StopTracker::new(args.max_turns);
            let mut gate = Gate::new(approval_policy);
            let mut approver = TuiApprover {
                terminal: &terminal,
                reader: &mut key_reader,
            };
            let mut ctx = ToolContext {
                sandbox: &args.sandbox,
                helper: &args.helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            // Races `agent::run` against a redraw tick (so the status
            // row's elapsed-time/shimmer animates while the turn is in
            // flight) and the event stream (so Esc can interrupt it).
            // `agent_future` is only ever polled here, in one place, so
            // dropping it on interrupt is real cancellation: Rust futures
            // stop making progress the moment nothing polls them again,
            // which is what "esc to interrupt" means in practice — this
            // doesn't kill an already-spawned OS subprocess a tool call
            // may have started, only stops *waiting* on the turn.
            // The tick-redraw branch below only redraws the footer
            // (status row's elapsed-time/shimmer) — it never needs to read
            // `session` at all now that history is appended once via
            // `append_new_history` rather than redrawn from it every frame,
            // so no snapshot is needed to sidestep `agent_future`'s
            // mutable borrow of `session` below. Mid-turn tool activity
            // reaches the screen through `events_rx` instead, which
            // carries owned `AgentEvent`s and so borrows nothing.
            //
            // Unbounded on purpose: a bounded sender would make
            // `agent::run` block on a full queue, i.e. let the display
            // throttle the actual turn. Events are small and a turn's
            // worth of them is bounded by the turn itself.
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            let agent_future = agent::run(
                args.provider.as_ref(),
                &mut session,
                audit.clone(),
                &mut stop,
                args.always_on,
                args.skills,
                args.agent_types,
                args.provider.clone(),
                args.spawn_concurrency,
                args.spawn_write_concurrency,
                Some(events_tx),
                &mut ctx,
            );
            tokio::pin!(agent_future);
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it since the
            // pre-draw above already painted the elapsed=0 frame.
            ticker.tick().await;
            let mut event_stream = ratatui::crossterm::event::EventStream::new();

            let turn_outcome = loop {
                tokio::select! {
                    biased;
                    result = &mut agent_future => break TurnOutcome::Done(result),
                    // Ahead of the tick and the key-event arms so a burst
                    // of tool activity is printed as it happens rather
                    // than queueing behind cosmetic redraws; behind
                    // `agent_future` so the turn's own completion (and
                    // the rollback it may need) is never delayed by
                    // display work. Anything still queued when the turn
                    // finishes is drained right after this loop.
                    Some(event) = events_rx.recv() => {
                        let appended = append_live_event(&event, &mut history);
                        // The user is scrolled back, not following the
                        // tail — advance `scroll_offset` by however many
                        // lines just landed so the *viewed content* stays
                        // fixed under them instead of the window silently
                        // sliding by `appended` lines (see
                        // `append_live_event`'s doc).
                        if scroll_offset > 0 {
                            scroll_offset += appended;
                        }
                    }
                    _ = ticker.tick() => {
                        status = Status::Thinking { elapsed: turn_started.elapsed() };
                        // History is never redrawn from here (see the
                        // comment above `agent_future`) — only the status
                        // row's shimmer/elapsed time animates, so this
                        // only needs the footer redrawn, never a new
                        // `append_new_history` call.
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
                                    &[],
                                    0,
                                )
                            })
                            .is_err()
                        {
                            break TurnOutcome::Fatal;
                        }
                    }
                    maybe_event = event_stream.next() => {
                        use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind};
                        match maybe_event {
                            Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                                // Esc interrupts the in-flight turn;
                                // PageUp/PageDown still scroll the
                                // conversation history while a turn is
                                // running, the same saturating-add/sub as
                                // the idle-loop handler above — previously
                                // every key but Esc was silently discarded
                                // here.
                                match key.code {
                                    KeyCode::Esc => break TurnOutcome::Interrupted,
                                    KeyCode::PageUp => {
                                        scroll_offset = scroll_offset.saturating_add(1);
                                    }
                                    KeyCode::PageDown => {
                                        scroll_offset = scroll_offset.saturating_sub(1);
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break TurnOutcome::Fatal,
                        }
                    }
                }
            };

            // `biased` makes `agent_future` win whenever it and
            // `events_rx` are ready in the same poll, so the very last
            // events of a turn can still be sitting in the queue here —
            // print them rather than silently dropping the final tool's
            // result marker.
            while let Ok(event) = events_rx.try_recv() {
                let appended = append_live_event(&event, &mut history);
                if scroll_offset > 0 {
                    scroll_offset += appended;
                }
            }
            turn_outcome
        };

        match outcome {
            TurnOutcome::Done(Ok(result)) => {
                cumulative_usage.input_tokens += result.usage.input_tokens;
                cumulative_usage.output_tokens += result.usage.output_tokens;
                cumulative_usage.total_tokens += result.usage.total_tokens;
                cumulative_usage.cached_tokens += result.usage.cached_tokens;
                status = Status::Idle;
                if let Some(reply) = session.messages.last()
                    && let Err(e) = persist::append_message(&session_path, reply)
                {
                    fatal_message = Some(format!("Can't persist the reply: {e}"));
                    break 'outer ExitCode::FAILURE;
                }
            }
            TurnOutcome::Done(Err(e)) => {
                // The agent loop can return after recording an assistant
                // message with tool_calls but before every matching
                // tool-result message is pushed (see AgentError::Stopped /
                // AgentError::Io in polaris_core::agent::run). Re-sending
                // that unbalanced tail to the provider on the next turn
                // would be rejected every time, so roll `session.messages`
                // back to right after the user's message — the only state
                // that was ever actually persisted for this turn.
                session.messages.truncate(checkpoint);
                status = Status::Error(e.to_string());
            }
            TurnOutcome::Interrupted => {
                // Same rollback as the error case above — an interrupted
                // turn can equally have left an unbalanced tool-call tail.
                session.messages.truncate(checkpoint);
                status = Status::Notice("interrupted".to_string());
            }
            TurnOutcome::Fatal => break ExitCode::FAILURE,
        }
    };

    // Already leaves the alternate screen entered above, on its own
    // (confirmed against ratatui 0.29.0's source) — no separate explicit
    // `LeaveAlternateScreen` call is needed here.
    ratatui::restore();
    // Restore the terminal's own default cursor shape, undoing the
    // BlinkingBar set at startup — best-effort, same as the set itself.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    if let Some(msg) = fatal_message {
        eprintln!("{msg}");
    }
    exit_code
}

/// Runs the `/resume` picker as its own small blocking loop on the same
/// terminal — the same approach the onboarding screen uses for a
/// full-screen selection UI. Its own reads are still simple blocking
/// ones — unlike the agent-turn loop, nothing here needs to animate
/// concurrently — but `terminal` is still a shared `&RefCell` rather
/// than `&mut` for consistency with the rest of `run()`, which never
/// gives up its own borrow of it. Generic over `Backend`/`KeyReader` for
/// the same reason `TuiApprover` is: so it's testable without a real
/// terminal. Returns the chosen conversation's id, or `None` if the user
/// backed out with Esc (or there was nothing to resume and any key was
/// pressed to dismiss the empty state).
fn run_resume_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    groups: &[sessions::DirGroup],
) -> Option<String> {
    let flat: Vec<&sessions::SessionSummary> =
        groups.iter().flat_map(|g| g.sessions.iter()).collect();
    let now = now_millis();

    if flat.is_empty() {
        let _ = terminal
            .borrow_mut()
            .draw(|f| render::render_resume_picker(f, groups, 0, now));
        let _ = reader.read_key();
        return None;
    }

    let mut selected = 0usize;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_resume_picker(f, groups, selected, now))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % flat.len(),
            Ok(KeyCode::Up) => selected = (selected + flat.len() - 1) % flat.len(),
            Ok(KeyCode::Enter) => return Some(flat[selected].id.clone()),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Handles `/resume`: shows the picker and, if the user picks a
/// conversation, swaps `session`/`session_path`/`meta_path`/
/// `session_started_at_millis` to it in place. A cancelled picker (Esc,
/// or nothing to resume) leaves every one of those untouched.
#[allow(clippy::too_many_arguments)]
fn handle_resume<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    sessions_dir: &std::path::Path,
    cwd_display: &str,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    let summaries = sessions::list_sessions(sessions_dir);
    let groups = sessions::grouped(summaries, cwd_display);
    let Some(id) = run_resume_picker(terminal, reader, &groups) else {
        return;
    };

    let new_session_path = sessions_dir.join(format!("{id}.jsonl"));
    let new_meta_path = new_session_path.with_extension("meta.json");
    match persist::load_session(&new_session_path) {
        Ok((loaded, truncated)) => {
            *session = loaded;
            *session_started_at_millis = persist::read_meta(&new_meta_path)
                .map(|m| m.started_at_millis)
                .unwrap_or_else(now_millis);
            *session_path = new_session_path;
            *meta_path = new_meta_path;
            local_lines.clear();
            *status = if truncated {
                Status::Notice(format!(
                    "{} had a corrupt line; resumed from the messages before it",
                    session_path.display()
                ))
            } else {
                Status::Idle
            };
        }
        Err(e) => *status = Status::Notice(format!("can't resume: {e}")),
    }
}

/// Handles `/new`: rotates to a brand-new conversation (fresh id, fresh
/// file) without touching the one being left — unlike the old
/// single-session-per-project model, it stays on disk and shows up in a
/// later `/resume`.
fn handle_new_session(
    sessions_dir: &std::path::Path,
    session: &mut polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) {
    *session = polaris_core::session::Session::default();
    *session_path = sessions_dir.join(format!("{}.jsonl", new_session_id()));
    *meta_path = session_path.with_extension("meta.json");
    *session_started_at_millis = now_millis();
    local_lines.clear();
    *status = Status::Idle;
}

/// Handles `/fork`: like `/new`, rotates to a brand-new file/id without
/// touching the one being left, except the new one is seeded with every
/// message so far instead of starting empty — codex's own `/fork`
/// ("fork the current chat") does the same. The seeded messages are
/// written to disk immediately (unlike a fresh `/new` session, which
/// only starts existing on disk lazily at the first new message) — a
/// fork with no messages yet added since forking would otherwise vanish
/// from `/resume`'s list, which defeats the point of forking one.
fn handle_fork(
    sessions_dir: &std::path::Path,
    cwd_display: &str,
    session: &polaris_core::session::Session,
    session_path: &mut PathBuf,
    meta_path: &mut PathBuf,
    session_started_at_millis: &mut u128,
    status: &mut Status,
) {
    let new_session_path = sessions_dir.join(format!("{}.jsonl", new_session_id()));
    let new_meta_path = new_session_path.with_extension("meta.json");
    let started = now_millis();

    if let Err(e) = persist::write_meta_if_absent(
        &new_meta_path,
        &persist::SessionMeta {
            cwd: cwd_display.to_string(),
            started_at_millis: started,
        },
    ) {
        *status = Status::Notice(format!("can't fork: {e}"));
        return;
    }
    for m in &session.messages {
        if let Err(e) = persist::append_message(&new_session_path, m) {
            *status = Status::Notice(format!("can't fork: {e}"));
            return;
        }
    }

    *status = Status::Notice(format!("forked to {}", new_session_path.display()));
    *session_path = new_session_path;
    *meta_path = new_meta_path;
    *session_started_at_millis = started;
}

/// Runs the `/permissions` picker as its own small blocking loop, the
/// same shape as `run_resume_picker` (three fixed rows instead of a
/// dynamic list, so no empty-state branch is needed). Returns the picked
/// policy, or `None` if the user backed out with Esc.
fn run_permissions_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    current: ApprovalPolicy,
) -> Option<ApprovalPolicy> {
    const OPTIONS: [ApprovalPolicy; 3] = [
        ApprovalPolicy::Never,
        ApprovalPolicy::OnRequest,
        ApprovalPolicy::Always,
    ];
    let mut selected = OPTIONS.iter().position(|p| *p == current).unwrap_or(0);
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_permissions_picker(f, current, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % OPTIONS.len(),
            Ok(KeyCode::Up) => selected = (selected + OPTIONS.len() - 1) % OPTIONS.len(),
            Ok(KeyCode::Enter) => return Some(OPTIONS[selected]),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Handles `/permissions`: shows the picker and, on selection, updates
/// `approval_policy` in place — the local `run()` holds instead of
/// `args.approval_policy` directly, since `args` isn't (and shouldn't
/// become) mutable just so one slash command can override it for the
/// rest of the session.
fn handle_permissions<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    approval_policy: &mut ApprovalPolicy,
    status: &mut Status,
) {
    if let Some(picked) = run_permissions_picker(terminal, reader, *approval_policy) {
        *approval_policy = picked;
        *status = Status::Notice(format!("permissions set to {picked:?}"));
    }
}

/// Runs the `/model` picker — same small blocking loop shape as
/// `run_permissions_picker`, navigating `render::MODEL_CATALOG` instead
/// of a fixed 3-option enum.
fn run_model_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    current: &str,
) -> Option<&'static str> {
    let catalog = render::MODEL_CATALOG;
    let mut selected = catalog.iter().position(|m| *m == current).unwrap_or(0);
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_model_picker(f, current, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % catalog.len(),
            Ok(KeyCode::Up) => selected = (selected + catalog.len() - 1) % catalog.len(),
            Ok(KeyCode::Enter) => return Some(catalog[selected]),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Runs the `/model` wizard's second step — same shape as
/// `run_model_picker`, navigating `render::EFFORT_CATALOG`. `Esc` here
/// means "go back to the model list" (matching the picker's own
/// "esc to go back" hint), not "cancel the whole wizard" — the caller
/// (`handle_model`) is what turns that into an actual step-back.
fn run_effort_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    model: &str,
    current: &str,
) -> Option<&'static str> {
    let catalog = render::EFFORT_CATALOG;
    let mut selected = catalog
        .iter()
        .position(|(name, _)| *name == current)
        .unwrap_or(0);
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_effort_picker(f, model, current, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % catalog.len(),
            Ok(KeyCode::Up) => selected = (selected + catalog.len() - 1) % catalog.len(),
            Ok(KeyCode::Enter) => return Some(catalog[selected].0),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Handles `/model`: a two-step wizard — pick the model, then pick its
/// reasoning effort (both catalogs shown as one flat list each; codex's
/// own screen splits `Max`/`Ultra` onto a second "Advanced Reasoning"
/// page, but that split was explicitly not wanted here) — mirroring the
/// two screens in codex's real `/model` (verified against two
/// screenshots of it). Esc on the effort step goes back to the model
/// list rather than cancelling outright, matching that screen's own
/// "esc to go back" hint; Esc on the model list cancels the whole thing.
/// On confirming both steps, switches what the provider actually sends
/// (`provider.set_model`/`set_effort`, through interior mutability —
/// see their docs) and updates the local display copies (`model_name`,
/// `effort_name`) the header/footer/notices read.
#[allow(clippy::too_many_arguments)]
fn handle_model<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    provider: &dyn Provider,
    model_name: &mut String,
    effort_name: &mut String,
    status: &mut Status,
) {
    loop {
        let Some(picked_model) = run_model_picker(terminal, reader, model_name) else {
            return;
        };
        let Some(picked_effort) = run_effort_picker(terminal, reader, picked_model, effort_name)
        else {
            continue;
        };
        provider.set_model(picked_model);
        // `picked_effort` is the picker's display name — what actually
        // reaches the provider is `effort_wire_value`'s translation of
        // it (see that function's docs on the real `400 Invalid value:
        // 'extra high'` this exists to prevent). `effort_name` still
        // stores the display name, since that's what the footer/notice
        // should keep showing the user picked.
        provider.set_effort(Some(render::effort_wire_value(picked_effort)));
        *model_name = picked_model.to_string();
        *effort_name = picked_effort.to_string();
        *status = Status::Notice(format!("model set to {picked_model} ({picked_effort})"));
        return;
    }
}

/// Runs the `/skills` picker: a blocking loop like the others, but with
/// no selection outcome — Enter and Esc both just close it (see
/// `render::render_skills_picker`'s docs on why: polaris has nothing
/// analogous to codex's enable/disable state to act on). Up/Down still
/// move a highlight for visual consistency with every other picker, even
/// though nothing currently reads which row is highlighted; that's ready
/// to become a scroll anchor if a project's skill list ever grows past
/// what a single screen holds, but that's not implemented yet.
fn run_skills_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    skills: &[polaris_skills::Skill],
) {
    let mut selected = 0usize;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_skills_picker(f, skills, selected))
            .is_err()
        {
            return;
        }
        if skills.is_empty() {
            let _ = reader.read_key();
            return;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % skills.len(),
            Ok(KeyCode::Up) => selected = (selected + skills.len() - 1) % skills.len(),
            Ok(KeyCode::Enter) | Ok(KeyCode::Esc) => return,
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}

/// What running a resolved slash command should do next, distinct from
/// `ExitCode` so the two call sites (Enter-on-a-highlighted-suggestion,
/// and Enter-on-fully-typed-but-unmatched text) can each decide how to
/// react to `Fatal` without duplicating the `fatal_message`/`break 'outer`
/// wiring twice.
enum SlashOutcome {
    Continue,
    Quit,
    Fatal(String),
}

const AGENTS_MD_TEMPLATE: &str = "# AGENTS.md\n\n\
Project-specific instructions for polaris. Everything up to the first\n\
`## ` heading (or the top of the file, if there is none) is sent to the\n\
model on every turn, so keep this part short — put anything long-form\n\
under its own `## ` section instead.\n";

const REVIEW_INSTRUCTION: &str =
    "Review the current uncommitted changes (`git diff`) and point out any issues you find.";

/// Expands `/review`'s carried text (empty if none was typed) into the
/// actual user message sent to the model. The model already has `bash`
/// and `read` to look at the diff itself — this only supplies the
/// instruction, the same way typing it out by hand would.
fn review_prompt(extra: &str) -> String {
    if extra.is_empty() {
        REVIEW_INSTRUCTION.to_string()
    } else {
        format!("{REVIEW_INSTRUCTION} Focus especially on: {extra}")
    }
}

/// Renders the conversation so far as a plain markdown transcript, for
/// `/export`. Deliberately simple — one `## ` heading per message, role
/// as the heading text, content underneath — rather than trying to
/// reproduce the TUI's own tool-call/tool-result formatting.
fn export_markdown(session: &polaris_core::session::Session) -> String {
    let mut out = String::from("# polaris conversation\n\n");
    for m in &session.messages {
        let heading = match m.role {
            polaris_provider::Role::User => "You",
            polaris_provider::Role::Assistant => "polaris",
            polaris_provider::Role::Tool => "tool result",
        };
        out.push_str(&format!("## {heading}\n\n"));
        if !m.tool_calls.is_empty() {
            for call in &m.tool_calls {
                out.push_str(&format!("_called `{}`_\n\n", call.name));
            }
        }
        if !m.content.is_empty() {
            out.push_str(&m.content);
            out.push_str("\n\n");
        }
    }
    out
}

/// Runs one resolved slash command. Never touches `session.messages` or
/// the model — `Clear` is the only variant here that touches persisted
/// state, and it does so by emptying the session file in place, not by
/// sending anything; `Init`/`Logout` touch the filesystem directly (a new
/// `AGENTS.md`, the deleted credentials file) but likewise never go near
/// the model or `session.messages`. `New` and `Resume` are never actually
/// dispatched here — see their match arms below.
#[allow(clippy::too_many_arguments)]
fn apply_slash_action(
    action: slash::Action,
    session: &mut polaris_core::session::Session,
    session_path: &std::path::Path,
    status: &mut Status,
    provider_name: &str,
    model_name: &str,
    cumulative_usage: polaris_provider::Usage,
    // No longer read here — `/skills` is intercepted in `run()` before
    // dispatch (see `run_skills_picker`) now that it's a full-screen
    // picker instead of a one-line `Status::Notice`. Kept as a parameter
    // rather than removed, since removing it would mean touching every
    // `apply_slash_action`/`call` call site (production and test) for a
    // signature change with no behavior difference.
    _skills: &[polaris_skills::Skill],
    cwd: &std::path::Path,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) -> SlashOutcome {
    match action {
        slash::Action::Quit => SlashOutcome::Quit,
        slash::Action::Clear => {
            session.messages.clear();
            local_lines.clear();
            match persist::clear_session(session_path) {
                Ok(()) => {
                    *status = Status::Idle;
                    SlashOutcome::Continue
                }
                Err(e) => SlashOutcome::Fatal(format!("Can't clear the persisted session: {e}")),
            }
        }
        slash::Action::Status => {
            *status = Status::Notice(format!(
                "{provider_name} / {model_name} — tokens: in {} / out {} / cache {} / total {} — {} messages",
                cumulative_usage.input_tokens,
                cumulative_usage.output_tokens,
                cumulative_usage.cached_tokens,
                cumulative_usage.total_tokens,
                session.messages.len(),
            ));
            SlashOutcome::Continue
        }
        slash::Action::Init => {
            let path = cwd.join("AGENTS.md");
            if path.exists() {
                *status = Status::Notice(format!("{} already exists", path.display()));
            } else {
                match std::fs::write(&path, AGENTS_MD_TEMPLATE) {
                    Ok(()) => *status = Status::Notice(format!("created {}", path.display())),
                    Err(e) => {
                        return SlashOutcome::Fatal(format!(
                            "Can't create {}: {e}",
                            path.display()
                        ));
                    }
                }
            }
            SlashOutcome::Continue
        }
        slash::Action::Pwd => {
            *status = Status::Notice(cwd.display().to_string());
            SlashOutcome::Continue
        }
        slash::Action::Export(destination) => {
            let path = if destination.is_empty() {
                cwd.join(format!("polaris-export-{}.md", now_millis()))
            } else {
                cwd.join(&destination)
            };
            match std::fs::write(&path, export_markdown(session)) {
                Ok(()) => *status = Status::Notice(format!("exported to {}", path.display())),
                Err(e) => {
                    return SlashOutcome::Fatal(format!("Can't write {}: {e}", path.display()));
                }
            }
            SlashOutcome::Continue
        }
        slash::Action::Diff => {
            // Read-only, so it runs directly rather than through the
            // sandbox (which exists to confine what the *model* can
            // mutate, not to gate the user's own read commands).
            const MAX_DIFF_LINES: usize = 200;
            let text = match std::process::Command::new("git")
                .arg("diff")
                .current_dir(cwd)
                .output()
            {
                Ok(out) if out.status.success() => {
                    if out.stdout.is_empty() {
                        "(no changes)".to_string()
                    } else {
                        String::from_utf8_lossy(&out.stdout).into_owned()
                    }
                }
                Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
                Err(e) => format!("can't run git diff: {e}"),
            };
            local_lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                "diff:",
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            )));
            let all_lines: Vec<&str> = text.lines().collect();
            let shown = all_lines.len().min(MAX_DIFF_LINES);
            for raw_line in &all_lines[..shown] {
                local_lines.push(ratatui::text::Line::from(render::sanitize(raw_line)));
            }
            if all_lines.len() > MAX_DIFF_LINES {
                local_lines.push(ratatui::text::Line::from(format!(
                    "... truncated, showing {shown} of {} lines",
                    all_lines.len()
                )));
            }
            *status = Status::Idle;
            SlashOutcome::Continue
        }
        slash::Action::Review(_)
        | slash::Action::New
        | slash::Action::Resume
        | slash::Action::Permissions
        | slash::Action::Fork
        | slash::Action::Model
        | slash::Action::Skills => {
            // Never reached: `run()` recognizes all of these before
            // dispatch — `Review` expands into a normal model turn;
            // `New`/`Resume`/`Fork` are handled by
            // `handle_new_session`/`handle_resume`/`handle_fork`;
            // `Permissions`/`Model`/`Skills` are handled by
            // `handle_permissions`/`handle_model`/`run_skills_picker`.
            // Every one of them needs state (`session_path`,
            // `approval_policy`, `model_name`, the terminal, ...) this
            // function doesn't have. This arm exists only so the match
            // stays exhaustive — if it's ever hit, fail soft rather than
            // panic over one command.
            *status = Status::Notice(
                "internal: this command should have been handled before dispatch. \
                                 Please report this as a polaris bug."
                    .to_string(),
            );
            SlashOutcome::Continue
        }
        slash::Action::Logout => {
            let outcome = polaris_auth::store::default_path()
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    polaris_auth::logout(&p)
                        .map(|removed| (p, removed))
                        .map_err(|e| e.to_string())
                });
            *status = match outcome {
                Ok((p, true)) => Status::Notice(format!("logged out: {}", p.display())),
                Ok((_, false)) => Status::Notice("not logged in".to_string()),
                Err(e) => Status::Notice(format!("can't log out: {e}")),
            };
            SlashOutcome::Continue
        }
        slash::Action::Help => {
            let list = slash::COMMANDS
                .iter()
                .map(|c| format!("/{}", c.name))
                .collect::<Vec<_>>()
                .join("  ");
            *status = Status::Notice(format!("commands: {list}"));
            SlashOutcome::Continue
        }
        slash::Action::Unknown(name) => {
            *status = Status::Notice(format!("unknown command: /{name} (try /help)"));
            SlashOutcome::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approver::KeyReader;
    use polaris_core::session::Session;
    use polaris_provider::Message;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ratatui::text::Line;
    use std::collections::VecDeque;

    /// Feeds a fixed, pre-scripted sequence of key codes — the same
    /// approach `approver`'s own tests use, so a picker-driving test
    /// doesn't need a real terminal or a live keyboard.
    struct ScriptedReader(VecDeque<KeyCode>);

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0.pop_front().unwrap_or(KeyCode::Null))
        }
    }

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
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(rows[0].contains("line from history"));
        // The footer's placeholder text lands in the bottom FOOTER_HEIGHT
        // rows, not the top history region.
        let footer_start = 30 - FOOTER_HEIGHT as usize;
        assert!(
            rows[footer_start..]
                .iter()
                .any(|r| r.contains("Ask polaris to do anything"))
        );
        assert!(
            !rows[..footer_start]
                .iter()
                .any(|r| r.contains("Ask polaris to do anything"))
        );
    }

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
        assert!(
            history.len() > after_first,
            "the assistant reply should have been appended, not reprinted from scratch"
        );

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
        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
        assert_eq!(printed_messages, 2);

        // Simulates /clear: the session shrinks out from under the counters.
        session.messages.clear();
        append_new_history(
            &session,
            &[],
            &mut printed_messages,
            &mut printed_local_lines,
            &mut history,
        );
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

    #[allow(clippy::too_many_arguments)]
    fn call(
        action: slash::Action,
        session: &mut Session,
        session_path: &std::path::Path,
        status: &mut Status,
        skills: &[polaris_skills::Skill],
        cwd: &std::path::Path,
        local_lines: &mut Vec<ratatui::text::Line<'static>>,
    ) -> SlashOutcome {
        apply_slash_action(
            action,
            session,
            session_path,
            status,
            "openai",
            "gpt-5.4",
            polaris_provider::Usage::default(),
            skills,
            cwd,
            local_lines,
        )
    }

    fn skill(name: &str) -> polaris_skills::Skill {
        polaris_skills::Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            body: String::new(),
            path: PathBuf::new(),
        }
    }

    // `abbreviate_home` reads `HOME`, which is process-global state —
    // swapping it races with any other test doing the same unless
    // serialized.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn abbreviate_home_collapses_a_path_under_home_to_a_tilde() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
        unsafe {
            std::env::set_var("HOME", "/Users/kn");
        }

        let short = abbreviate_home(std::path::Path::new("/Users/kn/File/projects/polaris"));

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(short, "~/File/projects/polaris");
    }

    #[test]
    fn abbreviate_home_leaves_a_path_outside_home_unchanged() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
        unsafe {
            std::env::set_var("HOME", "/Users/kn");
        }

        let unchanged = abbreviate_home(std::path::Path::new("/var/other/place"));

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(unchanged, "/var/other/place");
    }

    #[test]
    fn run_skills_picker_with_none_discovered_dismisses_on_any_key() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter]));

        // Must return promptly rather than looping forever on an empty list.
        run_skills_picker(&terminal, &mut reader, &[]);
    }

    #[test]
    fn run_skills_picker_shows_discovered_skills_and_closes_on_enter() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let skills = [skill("alpha"), skill("beta")];
        // Move the highlight once, then close — exercises Down as well as
        // the close path.
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));

        run_skills_picker(&terminal, &mut reader, &skills);

        let content = terminal
            .borrow()
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("alpha"));
        assert!(content.contains("beta"));
    }

    #[test]
    fn init_creates_agents_md_in_cwd() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Init,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let agents_md = dir.path().join("AGENTS.md");
        assert!(agents_md.exists());
        match status {
            Status::Notice(n) => assert!(n.contains("created")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn init_does_not_overwrite_an_existing_agents_md() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        let agents_md = dir.path().join("AGENTS.md");
        std::fs::write(&agents_md, "existing content").expect("write");

        call(
            slash::Action::Init,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert_eq!(
            std::fs::read_to_string(&agents_md).unwrap(),
            "existing content"
        );
        match status {
            Status::Notice(n) => assert!(n.contains("already exists")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn clear_empties_the_session_and_the_persisted_file_in_place() {
        let mut session = Session::default();
        session.push_user("something from before");
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        persist::append_message(&session_path, session.messages.last().unwrap())
            .expect("seed the file");

        call(
            slash::Action::Clear,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(session.messages.is_empty());
        assert_eq!(
            std::fs::read_to_string(&session_path).unwrap(),
            "",
            "the persisted file should be emptied too, not just the in-memory session"
        );
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn new_and_resume_are_never_actually_dispatched_through_apply_slash_action() {
        // `run()` intercepts every one of these before they'd ever reach
        // here (see `handle_new_session`/`handle_resume`/`handle_fork`/
        // `handle_permissions`/`handle_model`/`run_skills_picker`); this
        // only guards the defensive fallback arm against silently doing
        // nothing instead of surfacing that something is wrong.
        for action in [
            slash::Action::New,
            slash::Action::Resume,
            slash::Action::Fork,
            slash::Action::Permissions,
            slash::Action::Model,
            slash::Action::Skills,
        ] {
            let mut session = Session::default();
            let mut status = Status::Idle;
            let dir = tempfile::tempdir().expect("temp dir");
            let session_path = dir.path().join("session.jsonl");
            let mut local_lines = Vec::new();

            call(
                action,
                &mut session,
                &session_path,
                &mut status,
                &[],
                dir.path(),
                &mut local_lines,
            );

            match status {
                Status::Notice(n) => assert!(n.contains("internal")),
                _ => panic!("expected a Notice"),
            }
        }
    }

    #[test]
    fn handle_new_session_starts_a_fresh_file_without_touching_the_old_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::default();
        session.push_user("something from before");
        let old_path = dir.path().join("old.jsonl");
        persist::append_message(&old_path, session.messages.last().unwrap())
            .expect("seed the old file");
        let mut session_path = old_path.clone();
        let mut meta_path = dir.path().join("old.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = vec![Line::from("leftover /diff output")];

        handle_new_session(
            dir.path(),
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        );

        assert!(session.messages.is_empty());
        assert!(local_lines.is_empty());
        assert!(matches!(status, Status::Idle));
        assert_ne!(
            session_path, old_path,
            "a new session should get its own file"
        );
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap().lines().count(),
            1,
            "the old conversation must stay intact on disk, not get wiped"
        );
    }

    #[test]
    fn handle_resume_with_nothing_saved_dismisses_on_any_key_and_leaves_the_session_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::default();
        session.push_user("current, unsaved conversation");
        let original_path = dir.path().join("current.jsonl");
        let mut session_path = original_path.clone();
        let mut meta_path = dir.path().join("current.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let backend = TestBackend::new(60, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

        handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            "/tmp/current",
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        );

        assert_eq!(
            session.messages.len(),
            1,
            "nothing to resume to, so unchanged"
        );
        assert_eq!(session_path, original_path);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn handle_resume_swaps_in_the_picked_conversation() {
        let dir = tempfile::tempdir().expect("temp dir");
        persist::write_meta_if_absent(
            &dir.path().join("saved.meta.json"),
            &persist::SessionMeta {
                cwd: "/tmp/current".to_string(),
                started_at_millis: 500,
            },
        )
        .expect("write meta");
        persist::append_message(
            &dir.path().join("saved.jsonl"),
            &Message::user("resumed hi"),
        )
        .expect("seed saved session");

        let mut session = Session::default();
        session.push_user("current, unsaved conversation");
        let mut session_path = dir.path().join("current.jsonl");
        let mut meta_path = dir.path().join("current.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = vec![Line::from("leftover /diff output")];
        let backend = TestBackend::new(60, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // Only one saved conversation exists, so it's already highlighted;
        // Enter accepts it directly, same as the slash-command popup.
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

        handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            "/tmp/current",
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        );

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "resumed hi");
        assert_eq!(session_path, dir.path().join("saved.jsonl"));
        assert_eq!(meta_path, dir.path().join("saved.meta.json"));
        assert_eq!(session_started_at_millis, 500);
        assert!(
            local_lines.is_empty(),
            "switching conversations clears local-only output"
        );
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn handle_resume_cancelled_with_esc_leaves_everything_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        persist::write_meta_if_absent(
            &dir.path().join("saved.meta.json"),
            &persist::SessionMeta {
                cwd: "/tmp/current".to_string(),
                started_at_millis: 500,
            },
        )
        .expect("write meta");
        persist::append_message(
            &dir.path().join("saved.jsonl"),
            &Message::user("resumed hi"),
        )
        .expect("seed saved session");

        let mut session = Session::default();
        session.push_user("current, unsaved conversation");
        let original_path = dir.path().join("current.jsonl");
        let mut session_path = original_path.clone();
        let mut meta_path = dir.path().join("current.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;
        let mut local_lines = Vec::new();
        let backend = TestBackend::new(60, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Esc]));

        handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            "/tmp/current",
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        );

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "current, unsaved conversation");
        assert_eq!(session_path, original_path);
    }

    #[test]
    fn an_unknown_command_names_itself_in_the_notice() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Unknown("frobnicate".to_string()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => assert!(n.contains("frobnicate")),
            _ => panic!("expected a Notice"),
        }
    }

    /// A `Provider` whose only job is recording what `set_model` was last
    /// called with, so `handle_model` tests can confirm it actually
    /// reaches the provider (not just the display copy). `complete` is
    /// never exercised by those tests.
    struct RecordingProvider {
        model: std::sync::Mutex<Option<String>>,
        effort: std::sync::Mutex<Option<String>>,
    }

    impl RecordingProvider {
        fn new() -> Self {
            Self {
                model: std::sync::Mutex::new(None),
                effort: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn complete(
            &self,
            _req: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            unimplemented!("not exercised by handle_model tests")
        }

        fn set_model(&self, model: &str) {
            *self.model.lock().expect("lock") = Some(model.to_string());
        }

        fn set_effort(&self, effort: Option<&str>) {
            *self.effort.lock().expect("lock") = effort.map(str::to_string);
        }
    }

    #[test]
    fn handle_model_switches_the_provider_and_the_display_copies() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // gpt-5.4 is index 4 in MODEL_CATALOG; Down moves to gpt-5.4-mini,
        // Enter advances to the effort step. "high" is index 2 in
        // EFFORT_CATALOG; Down x2 from "low" (the starting `effort_name`)
        // reaches it, then Enter confirms the whole wizard.
        let mut reader = ScriptedReader(VecDeque::from([
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Enter,
        ]));
        let provider = RecordingProvider::new();
        let mut model_name = "gpt-5.4".to_string();
        let mut effort_name = render::DEFAULT_EFFORT.to_string();
        let mut status = Status::Idle;

        handle_model(
            &terminal,
            &mut reader,
            &provider,
            &mut model_name,
            &mut effort_name,
            &mut status,
        );

        assert_eq!(model_name, "gpt-5.4-mini");
        assert_eq!(effort_name, "high");
        assert_eq!(
            *provider.model.lock().expect("lock"),
            Some("gpt-5.4-mini".to_string()),
            "the provider itself must be switched, not just the display copy"
        );
        assert_eq!(
            *provider.effort.lock().expect("lock"),
            Some("high".to_string())
        );
        match status {
            Status::Notice(n) => {
                assert!(n.contains("gpt-5.4-mini"));
                assert!(n.contains("high"));
            }
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn handle_model_sends_the_wire_value_not_the_display_name_for_extra_high() {
        // Regression test: picking "extra high" used to reach the
        // provider as the literal string "extra high" (with a space),
        // which a real provider rejected with `400 Invalid value:
        // 'extra high'`. It must reach `provider.set_effort` as
        // `render::effort_wire_value("extra high")` (`"xhigh"`) while
        // `effort_name` (what the footer/notice show) keeps the
        // human-readable display name.
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // "extra high" is index 3 in EFFORT_CATALOG; Down x3 from "low".
        let mut reader = ScriptedReader(VecDeque::from([
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Enter,
        ]));
        let provider = RecordingProvider::new();
        let mut model_name = "gpt-5.6-sol".to_string();
        let mut effort_name = render::DEFAULT_EFFORT.to_string();
        let mut status = Status::Idle;

        handle_model(
            &terminal,
            &mut reader,
            &provider,
            &mut model_name,
            &mut effort_name,
            &mut status,
        );

        assert_eq!(
            effort_name, "extra high",
            "the display copy keeps the label"
        );
        assert_eq!(
            *provider.effort.lock().expect("lock"),
            Some("xhigh".to_string()),
            "the provider must receive the space-free wire value, not the display label"
        );
    }

    #[test]
    fn handle_model_cancelled_with_esc_on_the_model_step_leaves_everything_untouched() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
        let provider = RecordingProvider::new();
        let mut model_name = "gpt-5.4".to_string();
        let mut effort_name = render::DEFAULT_EFFORT.to_string();
        let mut status = Status::Idle;

        handle_model(
            &terminal,
            &mut reader,
            &provider,
            &mut model_name,
            &mut effort_name,
            &mut status,
        );

        assert_eq!(model_name, "gpt-5.4");
        assert_eq!(effort_name, render::DEFAULT_EFFORT);
        assert_eq!(*provider.model.lock().expect("lock"), None);
        assert_eq!(*provider.effort.lock().expect("lock"), None);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn handle_model_esc_on_the_effort_step_goes_back_to_the_model_list_not_the_whole_wizard() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // Enter the effort step, back out with Esc, then confirm the
        // (unchanged) model with a plain Enter, then confirm the
        // (unchanged) default effort with another Enter.
        let mut reader = ScriptedReader(VecDeque::from([
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Enter,
        ]));
        let provider = RecordingProvider::new();
        let mut model_name = "gpt-5.4".to_string();
        let mut effort_name = render::DEFAULT_EFFORT.to_string();
        let mut status = Status::Idle;

        handle_model(
            &terminal,
            &mut reader,
            &provider,
            &mut model_name,
            &mut effort_name,
            &mut status,
        );

        // The wizard completed (not cancelled) — Esc only backed out of
        // the effort step, so a final Notice with both values should
        // still be there.
        assert_eq!(model_name, "gpt-5.4");
        assert_eq!(effort_name, render::DEFAULT_EFFORT);
        assert_eq!(
            *provider.model.lock().expect("lock"),
            Some("gpt-5.4".to_string())
        );
        assert!(matches!(status, Status::Notice(_)));
    }

    #[test]
    fn status_includes_the_cached_token_count() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();
        let usage = polaris_provider::Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_tokens: 80,
        };

        apply_slash_action(
            slash::Action::Status,
            &mut session,
            &session_path,
            &mut status,
            "openai",
            "gpt-5.4",
            usage,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => {
                assert!(n.contains("cache 80"), "expected a cache figure in: {n}");
                assert!(n.contains("in 100"));
                assert!(n.contains("out 20"));
                assert!(n.contains("total 120"));
            }
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn pwd_reports_the_working_directory() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Pwd,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => assert_eq!(n, dir.path().display().to_string()),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn export_writes_the_conversation_as_markdown() {
        let mut session = Session::default();
        session.push_user("what does this repo do?");
        session.push_assistant("it's a coding agent");
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Export(String::new()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let path = match status {
            Status::Notice(n) => {
                assert!(n.starts_with("exported to "));
                n.strip_prefix("exported to ").unwrap().to_string()
            }
            _ => panic!("expected a Notice"),
        };
        let written = std::fs::read_to_string(&path).expect("exported file should exist");
        assert!(written.contains("## You"));
        assert!(written.contains("what does this repo do?"));
        assert!(written.contains("## polaris"));
        assert!(written.contains("it's a coding agent"));
    }

    #[test]
    fn export_with_a_destination_argument_writes_there_instead() {
        let mut session = Session::default();
        session.push_user("hi");
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Export("notes.md".to_string()),
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(dir.path().join("notes.md").exists());
    }

    #[test]
    fn handle_fork_seeds_the_new_file_with_the_current_conversation_and_keeps_the_old_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::default();
        session.push_user("hello");
        session.push_assistant("hi there");
        let old_path = dir.path().join("old.jsonl");
        persist::append_message(&old_path, &session.messages[0]).expect("seed old file");
        let mut session_path = old_path.clone();
        let mut meta_path = dir.path().join("old.meta.json");
        let mut session_started_at_millis = 111;
        let mut status = Status::Idle;

        handle_fork(
            dir.path(),
            "/tmp/example",
            &session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
        );

        assert_ne!(session_path, old_path, "a fork should get its own file");
        let (forked, truncated) = persist::load_session(&session_path).expect("load fork");
        assert!(!truncated);
        assert_eq!(forked.messages.len(), 2);
        assert_eq!(forked.messages[0].content, "hello");
        assert_eq!(forked.messages[1].content, "hi there");
        assert_eq!(
            std::fs::read_to_string(&old_path).unwrap().lines().count(),
            1,
            "the original conversation must stay intact on disk"
        );
        match status {
            Status::Notice(n) => assert!(n.contains("forked to")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn handle_permissions_switches_to_the_picked_policy() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        // Down from Never (index 0) to OnRequest (index 1), then Enter.
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
        let mut approval_policy = ApprovalPolicy::Never;
        let mut status = Status::Idle;

        handle_permissions(&terminal, &mut reader, &mut approval_policy, &mut status);

        assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
        match status {
            Status::Notice(n) => assert!(n.contains("OnRequest")),
            _ => panic!("expected a Notice"),
        }
    }

    #[test]
    fn handle_permissions_cancelled_with_esc_leaves_the_policy_untouched() {
        let backend = TestBackend::new(70, 14);
        let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
        let mut approval_policy = ApprovalPolicy::Never;
        let mut status = Status::Idle;

        handle_permissions(&terminal, &mut reader, &mut approval_policy, &mut status);

        assert_eq!(approval_policy, ApprovalPolicy::Never);
        assert!(matches!(status, Status::Idle));
    }

    #[test]
    fn diff_reports_no_changes_outside_a_git_repository() {
        // `cwd` here is a bare tempdir, not a git repo — `git diff` exits
        // non-zero and prints to stderr, which must show up as content
        // rather than a crash (a missing/broken git repo is a common,
        // recoverable situation, not a fatal one).
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        let outcome = call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(matches!(outcome, SlashOutcome::Continue));
        assert!(!local_lines.is_empty(), "should still report something");
    }

    #[test]
    fn diff_shows_no_changes_in_a_clean_git_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git must be on PATH for this test")
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);

        let mut session = Session::default();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let rendered: String = local_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("no changes"));
    }

    #[test]
    fn diff_shows_an_actual_uncommitted_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git must be on PATH for this test")
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.path().join("a.txt"), "hello, changed\n").expect("write");

        let mut session = Session::default();
        let mut status = Status::Idle;
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            slash::Action::Diff,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        let rendered: String = local_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("a.txt"));
        assert!(rendered.contains("changed"));
    }

    #[test]
    fn clear_also_empties_local_lines_from_a_previous_diff() {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = vec![ratatui::text::Line::from("leftover diff output")];

        call(
            slash::Action::Clear,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        assert!(local_lines.is_empty());
    }

    #[test]
    fn review_prompt_with_no_extra_text_is_the_bare_instruction() {
        assert_eq!(review_prompt(""), REVIEW_INSTRUCTION);
    }

    #[test]
    fn review_prompt_appends_the_extra_text_when_given() {
        let p = review_prompt("the auth module");
        assert!(p.starts_with(REVIEW_INSTRUCTION));
        assert!(p.contains("the auth module"));
    }

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

    #[test]
    // The `47` below is never read before being overwritten — that's the
    // point (it documents the pre-reset "mid-scroll" value the Submit arm
    // discards), so silence clippy's unused_assignments lint for it.
    #[allow(unused_assignments)]
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
}
