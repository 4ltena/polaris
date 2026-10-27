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
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

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

/// One numbered, two-line option: a marker + title row (highlighted when
/// selected) and a dim subtitle row underneath, mirroring the layout of
/// codex's own first-run screen without reusing its wording.
fn option_lines(
    number: u8,
    choice: Choice,
    selected: Choice,
    title: &str,
    subtitle: &str,
) -> [Line<'static>; 2] {
    let is_selected = choice == selected;
    let marker = if is_selected { "> " } else { "  " };
    let title_style = if is_selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    [
        Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{number}. {title}"), title_style),
        ]),
        Line::styled(
            format!("     {subtitle}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

pub fn render_choice_screen(frame: &mut Frame, selected: Choice) {
    let area = frame.area();

    let mut lines = vec![
        Line::styled(
            "Welcome to polaris, an improved harness built on codex",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("Sign in with ChatGPT to use polaris with your ChatGPT"),
        Line::from("subscription, or provide your own OpenAI API key for"),
        Line::from("usage-based billing."),
        Line::from(""),
    ];
    lines.extend(option_lines(
        1,
        Choice::ChatGpt,
        selected,
        "Sign in with ChatGPT",
        "Uses your existing ChatGPT Plus, Pro, Business, or Enterprise plan",
    ));
    lines.push(Line::from(""));
    lines.extend(option_lines(
        2,
        Choice::ApiKey,
        selected,
        "Provide your own API key",
        "Pay for what you use",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "\u{2191}/\u{2193} or 1/2 to choose, Enter to continue, Esc/Ctrl-C to cancel",
    ));

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

pub fn render_api_key_prompt(frame: &mut Frame, typed_len: usize) {
    render_secret_prompt(
        frame,
        typed_len,
        "Paste or type your OpenAI API key, then press Enter.\nEsc/Ctrl-C to cancel.",
        "API key",
    );
}

/// A masked single-line entry: only the character count is ever drawn.
fn render_secret_prompt(frame: &mut Frame, typed_len: usize, prompt: &str, title: &str) {
    let area = frame.area();
    let [prompt_area, input_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Length(3)]).areas(area);
    frame.render_widget(
        Paragraph::new(prompt.to_owned()).wrap(Wrap { trim: false }),
        prompt_area,
    );
    let masked: String = "*".repeat(typed_len);
    frame.render_widget(
        Paragraph::new(masked).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_owned()),
        ),
        input_area,
    );
}

/// Where the decider step saves the key and turns the decider on, and
/// whether a `local_only` setting rules the remote judge out.
#[derive(Debug, Clone)]
pub struct DeciderSetup {
    pub key_path: std::path::PathBuf,
    pub config_path: std::path::PathBuf,
    pub local_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeciderChoice {
    Jev,
    Skip,
}

pub const JEV_DISCLOSURE: &str =
    "Enabling Jev sends the conversation state to api.typesafe.ai to classify each turn.";

pub fn render_decider_screen(frame: &mut Frame, selected: DeciderChoice, local_only: bool) {
    let marker = |choice| if choice == selected { "> " } else { "  " };
    let style = |choice| {
        if choice == selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    };
    let mut lines = vec![
        Line::styled(
            "Choose a decider",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("A decider classifies each turn so polaris can move between workflow"),
        Line::from("phases (for example from general to implement). It is off by default."),
        Line::from(""),
    ];
    if local_only {
        lines.push(Line::styled(
            "Jev is unavailable: decision.local_only is set, and Jev is a remote service.",
            Style::default().fg(Color::Yellow),
        ));
    } else {
        lines.push(Line::from(vec![
            Span::raw(marker(DeciderChoice::Jev)),
            Span::styled("1. Jev (enter its API key)", style(DeciderChoice::Jev)),
        ]));
        lines.push(Line::styled(
            format!("     {JEV_DISCLOSURE}"),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::raw(marker(DeciderChoice::Skip)),
        Span::styled(
            if local_only {
                "Continue without a decider"
            } else {
                "2. No decider"
            },
            style(DeciderChoice::Skip),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(
        "\u{2191}/\u{2193} or 1/2 to choose, Enter to continue, Esc to skip",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("polaris")),
        frame.area(),
    );
}

/// What the decider step left behind. Only `Enabled` changes this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeciderResult {
    Skipped,
    Enabled,
    /// The key was saved, but the configuration was not changed; the text
    /// says why. Never carries the key.
    SavedOnly(String),
}

/// Saves the key and turns Jev on. Split from the screen so it is testable.
pub fn save_jev_and_enable(setup: &DeciderSetup, key: &str) -> Result<DeciderResult, String> {
    polaris_auth::decision_key::save_jev_to(&setup.key_path, key)
        .map_err(|error| format!("Could not save the decider key: {error}"))?;
    use polaris_core::decision_enable::EnableOutcome;
    let manual = "Add `[decision]` with `enabled = true` and `backend = \"jev\"` yourself.";
    Ok(
        match polaris_core::decision_enable::enable_jev(&setup.config_path) {
            Ok(EnableOutcome::Appended | EnableOutcome::Created) => DeciderResult::Enabled,
            Ok(EnableOutcome::AlreadyConfigured) => DeciderResult::SavedOnly(
                "config.toml already has a decision setting, so it was left unchanged.".into(),
            ),
            Ok(EnableOutcome::Unparsable) => DeciderResult::SavedOnly(format!(
                "config.toml could not be parsed, so it was left unchanged. {manual}"
            )),
            Ok(EnableOutcome::Symlink) => DeciderResult::SavedOnly(format!(
                "config.toml is a symlink, so it was left unchanged. {manual}"
            )),
            Err(error) => DeciderResult::SavedOnly(format!(
                "config.toml could not be updated ({error}). {manual}"
            )),
        },
    )
}

/// The decider step shown after a successful subscription sign-in and by
/// `/decider`. Sign-in already succeeded, so Esc, "No decider", a failed
/// draw or a save error all end the step without failing it.
pub fn decider_step<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    setup: &DeciderSetup,
    mut read: impl FnMut() -> Option<ratatui::crossterm::event::Event>,
    applies_now: bool,
) -> DeciderResult {
    use ratatui::crossterm::event::Event;
    let mut selected = if setup.local_only {
        DeciderChoice::Skip
    } else {
        DeciderChoice::Jev
    };
    loop {
        if terminal
            .draw(|f| render_decider_screen(f, selected, setup.local_only))
            .is_err()
        {
            return DeciderResult::Skipped;
        }
        // `None` is the end of input (cancel, shutdown or a read error).
        let Some(event) = read() else {
            return DeciderResult::Skipped;
        };
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Press && !setup.local_only {
            // The footer says 1/2 choose; here they select, not flip.
            match key.code {
                KeyCode::Char('1') => {
                    selected = DeciderChoice::Jev;
                    continue;
                }
                KeyCode::Char('2') => {
                    selected = DeciderChoice::Skip;
                    continue;
                }
                _ => {}
            }
        }
        match apply_choice_key(key) {
            ChoiceAction::Continue => continue,
            ChoiceAction::Toggle if !setup.local_only => {
                selected = match selected {
                    DeciderChoice::Jev => DeciderChoice::Skip,
                    DeciderChoice::Skip => DeciderChoice::Jev,
                };
                continue;
            }
            ChoiceAction::Toggle => continue,
            ChoiceAction::Quit => return DeciderResult::Skipped,
            ChoiceAction::Submit if selected == DeciderChoice::Skip => {
                return DeciderResult::Skipped;
            }
            ChoiceAction::Submit => {}
        }
        let mut typed = String::new();
        let key = loop {
            let prompt = "Paste or type your Jev API key, then press Enter.\n\
                          It is saved to ~/.polaris/decision_keys.json (owner-only), never to .env. Esc to skip.";
            if terminal
                .draw(|f| render_secret_prompt(f, typed.chars().count(), prompt, "Jev API key"))
                .is_err()
            {
                return DeciderResult::Skipped;
            }
            let Some(event) = read() else {
                return DeciderResult::Skipped;
            };
            let event = match event {
                Event::Key(event) => event,
                // Inside the chat TUI bracketed paste is on, so a pasted key
                // arrives whole rather than as keystrokes.
                Event::Paste(text) => {
                    // A newline or escape in a pasted key would only fail later,
                    // at header construction; refuse it here.
                    if !text.trim().chars().any(char::is_control) {
                        typed.push_str(text.trim());
                    }
                    continue;
                }
                _ => continue,
            };
            match apply_key_entry_key(&mut typed, event) {
                KeyEntryAction::Continue => continue,
                KeyEntryAction::Quit => return DeciderResult::Skipped,
                KeyEntryAction::Submit(key) => break key,
            }
        };
        let result = save_jev_and_enable(setup, &key);
        drop(key);
        let when = if applies_now {
            "It takes effect now."
        } else {
            "It takes effect from the next launch."
        };
        let (message, outcome) = match result {
            Ok(DeciderResult::Enabled) => (
                format!("Jev is enabled and its key is saved. {when}"),
                DeciderResult::Enabled,
            ),
            Ok(DeciderResult::SavedOnly(reason)) => (
                format!("The key is saved. {reason}"),
                DeciderResult::SavedOnly(reason),
            ),
            Ok(DeciderResult::Skipped) => return DeciderResult::Skipped,
            Err(reason) => (reason, DeciderResult::Skipped),
        };
        let _ = terminal.draw(|f| {
            f.render_widget(
                Paragraph::new(format!("{message}\n\nPress any key to continue."))
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title("polaris")),
                f.area(),
            )
        });
        while let Some(event) = read() {
            if matches!(event, Event::Key(_)) {
                break;
            }
        }
        return outcome;
    }
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
        KeyCode::Esc => ChoiceAction::Quit,
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
        KeyCode::Esc => KeyEntryAction::Quit,
        KeyCode::Enter => {
            if buffer.trim().is_empty() {
                KeyEntryAction::Continue
            } else {
                KeyEntryAction::Submit(std::mem::take(buffer).trim().to_string())
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

#[cfg(target_os = "linux")]
use std::io;
use std::io::IsTerminal;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::terminal::DefaultTerminal;
#[cfg(target_os = "linux")]
use ratatui::Terminal;
#[cfg(target_os = "linux")]
use ratatui::backend::Backend;
#[cfg(all(target_os = "linux", test))]
use ratatui::backend::CrosstermBackend;
#[cfg(target_os = "linux")]
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Which credential became available once onboarding completes
/// successfully. `polaris-cli::main()` uses this to decide which
/// provider arm to retry.
#[derive(Debug)]
pub enum Outcome {
    ApiKeySaved,
    CodexLoggedIn,
    /// Signed in, and the decider step turned Jev on: the caller reloads
    /// the decision settings before it builds the decider.
    CodexLoggedInDeciderEnabled,
}

fn signed_in(result: DeciderResult) -> Outcome {
    if result == DeciderResult::Enabled {
        Outcome::CodexLoggedInDeciderEnabled
    } else {
        Outcome::CodexLoggedIn
    }
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
                write!(
                    f,
                    "polaris: refusing to start onboarding on a non-interactive terminal"
                )
            }
            OnboardingError::Cancelled => write!(f, "onboarding cancelled"),
            OnboardingError::Auth(msg) => write!(f, "{msg}"),
        }
    }
}

#[cfg(target_os = "linux")]
fn shutdown_requested() -> bool {
    crate::signal::pending_linux_process_shutdown().is_some()
}

#[cfg(target_os = "linux")]
fn enter_onboarding_terminal_with<T>(
    pending_shutdown: impl FnOnce() -> bool,
    enter: impl FnOnce() -> Result<T, OnboardingError>,
) -> Result<T, OnboardingError> {
    if pending_shutdown() {
        Err(OnboardingError::Cancelled)
    } else {
        enter()
    }
}

#[cfg(target_os = "linux")]
fn save_api_key_after_observation<E: std::fmt::Display>(
    pending_shutdown: impl FnOnce() -> bool,
    save: impl FnOnce() -> Result<(), E>,
) -> Result<Outcome, OnboardingError> {
    if pending_shutdown() {
        return Err(OnboardingError::Cancelled);
    }
    // A completed save remains committed even if a signal arrives in flight.
    save()
        .map(|()| Outcome::ApiKeySaved)
        .map_err(|error| OnboardingError::Auth(error.to_string()))
}

#[cfg(target_os = "linux")]
fn reenter_after_oauth_success<T>(
    mut pending_shutdown: impl FnMut() -> bool,
    enter: impl FnOnce() -> Result<T, OnboardingError>,
) -> Result<Option<T>, OnboardingError> {
    // A completed login remains a success; pending shutdown only suppresses
    // the former redundant terminal re-entry before returning to the caller.
    if pending_shutdown() || pending_shutdown() {
        return Ok(None);
    }
    enter().map(Some)
}

#[cfg(target_os = "linux")]
fn reenter_after_oauth_error<T>(
    auth_error: &str,
    mut pending_shutdown: impl FnMut() -> bool,
    enter: impl FnOnce() -> Result<T, OnboardingError>,
) -> Result<T, OnboardingError> {
    // The OAuth call is opaque. Observe immediately after its error and again
    // before enabling raw mode; keep the call's actual error if interrupted.
    if pending_shutdown() || pending_shutdown() {
        return Err(OnboardingError::Auth(auth_error.to_string()));
    }
    enter()
}

/// The decider step after a successful sign-in. Esc and "No decider" still
/// end a successful login, but a real exit request during the step is not
/// swallowed: the login is already saved, so cancelling here loses nothing.
#[cfg(target_os = "linux")]
fn decider_after_login(
    terminal: &mut DefaultTerminal,
    decider: &DeciderSetup,
) -> Result<Outcome, OnboardingError> {
    let result = decider_step(terminal, decider, || read_onboarding_event().ok(), true);
    if shutdown_requested() {
        return Err(OnboardingError::Cancelled);
    }
    Ok(signed_in(result))
}

#[cfg(target_os = "linux")]
fn read_onboarding_event() -> Result<ratatui::crossterm::event::Event, OnboardingError> {
    loop {
        if shutdown_requested() {
            return Err(OnboardingError::Cancelled);
        }
        let ready = match ratatui::crossterm::event::poll(Duration::from_millis(50)) {
            Ok(ready) => ready,
            Err(_) if shutdown_requested() => return Err(OnboardingError::Cancelled),
            Err(_) => return Err(OnboardingError::Auth("failed to read a key".to_string())),
        };
        if shutdown_requested() {
            return Err(OnboardingError::Cancelled);
        }
        if ready {
            break;
        }
    }

    let event = match crate::terminal::read_event() {
        Ok(event) => event,
        Err(_) if shutdown_requested() => return Err(OnboardingError::Cancelled),
        Err(_) => return Err(OnboardingError::Auth("failed to read a key".to_string())),
    };
    if shutdown_requested() {
        Err(OnboardingError::Cancelled)
    } else {
        Ok(event)
    }
}

/// Release every acquired terminal state, retaining the first cleanup error.
#[cfg(target_os = "linux")]
fn restore_acquired_terminal<B: Backend>(
    terminal: &mut Option<Terminal<B>>,
    cursor_hidden: &mut bool,
    raw: &mut bool,
    alternate: &mut bool,
    mut disable_raw: impl FnMut() -> io::Result<()>,
    mut leave_alternate: impl FnMut() -> io::Result<()>,
) -> Result<(), OnboardingError> {
    let mut first_error = None;
    if let Some(mut terminal) = terminal.take() {
        if std::mem::take(cursor_hidden) {
            if let Err(error) = terminal.show_cursor() {
                first_error = Some(error);
                // ratatui retains its hidden flag when Show fails, so Drop
                // would issue a second Show. This terminal is no longer used;
                // the caller receives a cleanup error. Leak only this
                // instance to keep the one attempted restoration.
                std::mem::forget(terminal);
            } else {
                drop(terminal);
            }
        } else {
            drop(terminal);
        }
    }
    if *raw {
        *raw = false;
        if let Err(error) = disable_raw() {
            first_error.get_or_insert(error);
        }
    }
    if *alternate {
        *alternate = false;
        if let Err(error) = leave_alternate() {
            first_error.get_or_insert(error);
        }
    }
    first_error
        .map(|error| {
            Err(OnboardingError::Auth(format!(
                "failed to restore terminal: {error}"
            )))
        })
        .unwrap_or(Ok(()))
}

/// Owns raw mode, the alternate screen and cursor visibility. Onboarding
/// uses the shared backend for mouse selection and never changes cursor shape.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct OnboardingTerminal {
    terminal: Option<DefaultTerminal>,
    cursor_hidden: bool,
    raw: bool,
    alternate: bool,
}

#[cfg(target_os = "linux")]
impl OnboardingTerminal {
    fn enter() -> Result<Self, OnboardingError> {
        let mut owner = Self::default();
        enable_raw_mode().map_err(|error| OnboardingError::Auth(error.to_string()))?;
        owner.raw = true;
        ratatui::crossterm::execute!(io::stdout(), EnterAlternateScreen)
            .map_err(|error| OnboardingError::Auth(error.to_string()))?;
        owner.alternate = true;
        let mut terminal = Terminal::new(crate::terminal::backend())
            .map_err(|error| OnboardingError::Auth(error.to_string()))?;
        terminal
            .hide_cursor()
            .map_err(|error| OnboardingError::Auth(error.to_string()))?;
        owner.cursor_hidden = true;
        owner.terminal = Some(terminal);
        Ok(owner)
    }

    fn terminal_mut(&mut self) -> &mut DefaultTerminal {
        self.terminal
            .as_mut()
            .expect("onboarding terminal is active")
    }

    fn restore(&mut self) -> Result<(), OnboardingError> {
        restore_acquired_terminal(
            &mut self.terminal,
            &mut self.cursor_hidden,
            &mut self.raw,
            &mut self.alternate,
            disable_raw_mode,
            || ratatui::crossterm::execute!(io::stdout(), LeaveAlternateScreen),
        )
    }
}

#[cfg(target_os = "linux")]
impl Drop for OnboardingTerminal {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("{error}");
        }
    }
}

enum LoginMode<'a> {
    Legacy(&'a Path),
    Registered(std::sync::Arc<polaris_auth::accounts::owner::AccountOwner>),
}

pub async fn run(
    auth_store_path: &Path,
    api_key_path: &Path,
    decider: &DeciderSetup,
) -> Result<Outcome, OnboardingError> {
    run_mode(LoginMode::Legacy(auth_store_path), api_key_path, decider).await
}

/// Reconnect logged-out registrations without falling back to the legacy store.
pub async fn run_registered(
    owner: std::sync::Arc<polaris_auth::accounts::owner::AccountOwner>,
    api_key_path: &Path,
    decider: &DeciderSetup,
) -> Result<Outcome, OnboardingError> {
    if cfg!(not(unix)) {
        return Err(OnboardingError::Auth(
            "protected account storage is unavailable".into(),
        ));
    }
    run_mode(LoginMode::Registered(owner), api_key_path, decider).await
}

pub(crate) fn render_registered_wait(frame: &mut Frame) {
    frame.render_widget(
        Paragraph::new("Authorize in your browser. Esc / Ctrl-C: cancel.").block(
            Block::default()
                .borders(Borders::ALL)
                .title("polaris login"),
        ),
        frame.area(),
    );
}

#[cfg(unix)]
async fn register(
    owner: &std::sync::Arc<polaris_auth::accounts::owner::AccountOwner>,
    terminal: &mut crate::terminal::DefaultTerminal,
) -> Result<Outcome, OnboardingError> {
    let (ready, after_callback) = tokio::sync::oneshot::channel();
    owner
        .register(
            None,
            |url, state| async move {
                let result = crate::auth_browser::callback(url, state, |url, status, scroll| {
                    crate::auth_browser::draw_link(terminal, url, status, scroll)
                })
                .await;
                let _ = ready.send(());
                result
            },
            crate::auth_browser::cancel_after_callback(after_callback),
        )
        .await
        .map(|_| Outcome::CodexLoggedIn)
        .map_err(|error| match error {
            polaris_auth::AuthError::Denied(reason) if reason == "cancelled" => {
                OnboardingError::Cancelled
            }
            other => OnboardingError::Auth(other.to_string()),
        })
}

#[cfg(not(unix))]
async fn register(
    _owner: &std::sync::Arc<polaris_auth::accounts::owner::AccountOwner>,
    _terminal: &mut crate::terminal::DefaultTerminal,
) -> Result<Outcome, OnboardingError> {
    Err(OnboardingError::Auth(
        "protected account storage is unavailable".into(),
    ))
}

async fn legacy_subscription(
    auth_store_path: &Path,
    terminal: &mut crate::terminal::DefaultTerminal,
) -> Result<polaris_auth::Credentials, polaris_auth::AuthError> {
    let exchanging = std::rc::Rc::new(std::cell::Cell::new(false));
    let phase = exchanging.clone();
    let login = polaris_auth::login::run_with_callback(
        polaris_auth::ISSUER,
        auth_store_path,
        |url, state| async move {
            let result = crate::auth_browser::callback(url, state, |url, status, scroll| {
                crate::auth_browser::draw_link(terminal, url, status, scroll)
            })
            .await;
            phase.set(result.is_ok());
            result
        },
    );
    tokio::pin!(login);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        tokio::select! {
            result = &mut login => return result,
            _ = tick.tick(), if exchanging.get() => {
                // Keep visible-text gestures alive without abandoning the
                // existing legacy token exchange or changing its cancel policy.
                if ratatui::crossterm::event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                    let _ = crate::terminal::read_event();
                }
            }
        }
    }
}

/// Shows the onboarding screen and drives it to completion. Owns the
/// terminal for its own lifetime (enters/leaves the alternate screen
/// through an owned guard), independent of
/// whatever the caller does with the terminal afterward — the chat loop's
/// own `run()` re-inits the terminal fresh once this returns.
#[cfg(target_os = "linux")]
async fn run_mode(
    mode: LoginMode<'_>,
    api_key_path: &Path,
    decider: &DeciderSetup,
) -> Result<Outcome, OnboardingError> {
    let mut terminal = enter_onboarding_terminal_with(shutdown_requested, || {
        if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
            return Err(OnboardingError::NonInteractive);
        }
        OnboardingTerminal::enter()
    })?;
    let mut selected = Choice::ChatGpt;

    let outcome = 'outer: loop {
        if shutdown_requested() {
            break 'outer Err(OnboardingError::Cancelled);
        }
        if terminal
            .terminal_mut()
            .draw(|f| render_choice_screen(f, selected))
            .is_err()
        {
            break 'outer Err(OnboardingError::Auth(
                "failed to draw the terminal".to_string(),
            ));
        }
        let event = match read_onboarding_event() {
            Ok(e) => e,
            Err(error) => break 'outer Err(error),
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
                let LoginMode::Legacy(auth_store_path) = &mode else {
                    let LoginMode::Registered(owner) = &mode else {
                        unreachable!()
                    };
                    if terminal
                        .terminal_mut()
                        .draw(render_registered_wait)
                        .is_err()
                    {
                        break 'outer Err(OnboardingError::Auth(
                            "failed to draw the terminal".into(),
                        ));
                    }
                    let result = register(owner, terminal.terminal_mut()).await;
                    break 'outer match result {
                        Ok(_) => decider_after_login(terminal.terminal_mut(), decider),
                        Err(error) => Err(error),
                    };
                };
                if shutdown_requested() {
                    break 'outer Err(OnboardingError::Cancelled);
                }
                let result = legacy_subscription(auth_store_path, terminal.terminal_mut()).await;
                if let Err(error) = terminal.restore() {
                    break 'outer Err(error);
                }
                if matches!(&result, Err(polaris_auth::AuthError::Denied(reason)) if reason == "cancelled")
                {
                    break 'outer Err(OnboardingError::Cancelled);
                }
                match result {
                    Ok(_) => {
                        terminal = match reenter_after_oauth_success(
                            shutdown_requested,
                            OnboardingTerminal::enter,
                        ) {
                            Ok(Some(terminal)) => terminal,
                            Ok(None) => break 'outer Ok(Outcome::CodexLoggedIn),
                            Err(error) => break 'outer Err(error),
                        };
                        break 'outer decider_after_login(terminal.terminal_mut(), decider);
                    }
                    Err(e) => {
                        // Stay on the choice screen and let the user try
                        // again rather than exiting the whole process over
                        // one failed sign-in attempt (e.g. the user closed
                        // the browser tab without authorizing).
                        let message = e.to_string();
                        terminal = match reenter_after_oauth_error(
                            &message,
                            shutdown_requested,
                            OnboardingTerminal::enter,
                        ) {
                            Ok(terminal) => terminal,
                            Err(error) => break 'outer Err(error),
                        };
                        if terminal
                            .terminal_mut()
                            .draw(|f| {
                                let area = f.area();
                                f.render_widget(
                                    ratatui::widgets::Paragraph::new(format!(
                                        "sign-in failed: {message}\n\npress any key to try again"
                                    )),
                                    area,
                                )
                            })
                            .is_err()
                        {
                            break 'outer Err(OnboardingError::Auth(message));
                        }
                        if let Err(error) = read_onboarding_event() {
                            break 'outer Err(error);
                        }
                        continue;
                    }
                }
            }
            Choice::ApiKey => {
                let mut typed = String::new();
                let key = 'entry: loop {
                    if shutdown_requested() {
                        break 'outer Err(OnboardingError::Cancelled);
                    }
                    if terminal
                        .terminal_mut()
                        .draw(|f| render_api_key_prompt(f, typed.chars().count()))
                        .is_err()
                    {
                        break 'outer Err(OnboardingError::Auth(
                            "failed to draw the terminal".to_string(),
                        ));
                    }
                    let event = match read_onboarding_event() {
                        Ok(e) => e,
                        Err(error) => break 'outer Err(error),
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
                break 'outer save_api_key_after_observation(shutdown_requested, || {
                    polaris_auth::api_key::save_to(api_key_path, &key)
                });
            }
        }
    };

    terminal.restore()?;
    outcome
}

/// Preserve the existing non-Linux onboarding behavior.
#[cfg(not(target_os = "linux"))]
async fn run_mode(
    mode: LoginMode<'_>,
    api_key_path: &Path,
    decider: &DeciderSetup,
) -> Result<Outcome, OnboardingError> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(OnboardingError::NonInteractive);
    }

    let mut terminal = crate::terminal::init();
    let mut selected = Choice::ChatGpt;

    let outcome = 'outer: loop {
        if terminal
            .draw(|f| render_choice_screen(f, selected))
            .is_err()
        {
            break 'outer Err(OnboardingError::Auth(
                "failed to draw the terminal".to_string(),
            ));
        }
        let event = match crate::terminal::read_event() {
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
                let LoginMode::Legacy(auth_store_path) = &mode else {
                    let LoginMode::Registered(owner) = &mode else {
                        unreachable!()
                    };
                    if terminal.draw(render_registered_wait).is_err() {
                        break 'outer Err(OnboardingError::Auth(
                            "failed to draw the terminal".into(),
                        ));
                    }
                    let result = register(owner, &mut terminal).await;
                    break 'outer result.map(|_| {
                        signed_in(decider_step(
                            &mut terminal,
                            decider,
                            || crate::terminal::read_event().ok(),
                            true,
                        ))
                    });
                };
                let result = legacy_subscription(auth_store_path, &mut terminal).await;
                crate::terminal::restore();
                if matches!(&result, Err(polaris_auth::AuthError::Denied(reason)) if reason == "cancelled")
                {
                    break 'outer Err(OnboardingError::Cancelled);
                }
                terminal = crate::terminal::init();
                match result {
                    Ok(_) => {
                        break 'outer Ok(signed_in(decider_step(
                            &mut terminal,
                            decider,
                            || crate::terminal::read_event().ok(),
                            true,
                        )));
                    }
                    Err(e) => {
                        // Stay on the choice screen and let the user try
                        // again rather than exiting the whole process over
                        // one failed sign-in attempt (e.g. the user closed
                        // the browser tab without authorizing).
                        if terminal
                            .draw(|f| {
                                let area = f.area();
                                f.render_widget(
                                    ratatui::widgets::Paragraph::new(format!(
                                        "sign-in failed: {e}\n\npress any key to try again"
                                    )),
                                    area,
                                )
                            })
                            .is_err()
                        {
                            break 'outer Err(OnboardingError::Auth(e.to_string()));
                        }
                        let _ = crate::terminal::read_event();
                        continue;
                    }
                }
            }
            Choice::ApiKey => {
                let mut typed = String::new();
                let key = 'entry: loop {
                    if terminal
                        .draw(|f| render_api_key_prompt(f, typed.chars().count()))
                        .is_err()
                    {
                        break 'outer Err(OnboardingError::Auth(
                            "failed to draw the terminal".to_string(),
                        ));
                    }
                    let event = match crate::terminal::read_event() {
                        Ok(e) => e,
                        Err(_) => {
                            break 'outer Err(OnboardingError::Auth(
                                "failed to read a key".to_string(),
                            ));
                        }
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

    crate::terminal::restore();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    #[cfg(target_os = "linux")]
    use std::cell::Cell;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_onboarding_pre_raw_shutdown_skips_terminal_and_auth() {
        let entered_raw = Cell::new(false);
        let started_auth = Cell::new(false);
        let result = (|| {
            enter_onboarding_terminal_with(
                || true,
                || {
                    entered_raw.set(true);
                    Ok(())
                },
            )?;
            started_auth.set(true);
            Ok::<(), OnboardingError>(())
        })();
        assert!(matches!(result, Err(OnboardingError::Cancelled)));
        assert!(!entered_raw.get());
        assert!(!started_auth.get());

        // A signal after enter but before draw must restore the cursor, raw
        // mode and alternate screen exactly once. The same production release
        // function uses a real ratatui Terminal and a recording backend here.
        use std::cell::RefCell;
        use std::io::Write;
        use std::rc::Rc;

        struct RecordingWriter {
            bytes: Rc<RefCell<Vec<u8>>>,
            fail_next_write: Rc<Cell<bool>>,
            show_attempts: Rc<Cell<usize>>,
        }
        impl Write for RecordingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let is_show = bytes
                    .windows(b"\x1b[?25h".len())
                    .any(|part| part == b"\x1b[?25h");
                if is_show || self.fail_next_write.get() {
                    self.show_attempts.set(self.show_attempts.get() + 1);
                }
                if self.fail_next_write.replace(false) {
                    return Err(io::Error::other("injected cursor Show failure"));
                }
                self.bytes.borrow_mut().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let make_terminal = |bytes: &Rc<RefCell<Vec<u8>>>,
                             fail: &Rc<Cell<bool>>,
                             show_attempts: &Rc<Cell<usize>>| {
            Terminal::with_options(
                CrosstermBackend::new(RecordingWriter {
                    bytes: Rc::clone(bytes),
                    fail_next_write: Rc::clone(fail),
                    show_attempts: Rc::clone(show_attempts),
                }),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 16)),
                },
            )
            .expect("recording terminal")
        };
        let sequence_count = |bytes: &[u8], sequence: &[u8]| {
            bytes
                .windows(sequence.len())
                .filter(|part| *part == sequence)
                .count()
        };
        let hide = b"\x1b[?25l";
        let show = b"\x1b[?25h";

        let bytes = Rc::new(RefCell::new(Vec::new()));
        let fail = Rc::new(Cell::new(false));
        let show_attempts = Rc::new(Cell::new(0));
        let mut terminal = make_terminal(&bytes, &fail, &show_attempts);
        terminal.hide_cursor().expect("acquire hidden cursor");
        let mut acquired_terminal = Some(terminal);
        let mut cursor_hidden = true;
        let mut raw = true;
        let mut alternate = true;
        let raw_cleanup = Cell::new(0);
        let alternate_cleanup = Cell::new(0);
        let pending_shutdown = Cell::new(false);
        pending_shutdown.set(true); // signal after enter and before the first draw
        let mut restore = || {
            restore_acquired_terminal(
                &mut acquired_terminal,
                &mut cursor_hidden,
                &mut raw,
                &mut alternate,
                || {
                    raw_cleanup.set(raw_cleanup.get() + 1);
                    Ok(())
                },
                || {
                    alternate_cleanup.set(alternate_cleanup.get() + 1);
                    Ok(())
                },
            )
        };
        let shutdown_restore = if pending_shutdown.get() {
            restore()
        } else {
            panic!("signal must be pending before the first draw");
        };
        shutdown_restore.expect("signal restore");
        restore().expect("idempotent restore");
        let recorded = bytes.borrow();
        assert_eq!(sequence_count(&recorded, hide), 1);
        assert_eq!(sequence_count(&recorded, show), 1);
        assert_eq!(show_attempts.get(), 1);
        assert_eq!(raw_cleanup.get(), 1);
        assert_eq!(alternate_cleanup.get(), 1);
        drop(recorded);

        // A failed cursor Show must become a reportable cleanup error while
        // the raw and alternate restorations are still attempted.
        let failing_bytes = Rc::new(RefCell::new(Vec::new()));
        let fail_next_write = Rc::new(Cell::new(false));
        let failing_show_attempts = Rc::new(Cell::new(0));
        let mut terminal = make_terminal(&failing_bytes, &fail_next_write, &failing_show_attempts);
        terminal.hide_cursor().expect("acquire hidden cursor");
        let mut acquired_terminal = Some(terminal);
        let mut cursor_hidden = true;
        let mut raw = true;
        let mut alternate = true;
        let raw_cleanup = Cell::new(0);
        let alternate_cleanup = Cell::new(0);
        fail_next_write.set(true);
        let error = restore_acquired_terminal(
            &mut acquired_terminal,
            &mut cursor_hidden,
            &mut raw,
            &mut alternate,
            || {
                raw_cleanup.set(raw_cleanup.get() + 1);
                Ok(())
            },
            || {
                alternate_cleanup.set(alternate_cleanup.get() + 1);
                Ok(())
            },
        )
        .expect_err("cursor Show failure must win over clean signal status");
        assert!(matches!(
            error,
            OnboardingError::Auth(message) if message.contains("injected cursor Show failure")
        ));
        assert_eq!(raw_cleanup.get(), 1);
        assert_eq!(alternate_cleanup.get(), 1);
        assert_eq!(failing_show_attempts.get(), 1);
        assert_eq!(sequence_count(&failing_bytes.borrow(), show), 0);
        assert!(!cursor_hidden);
        assert!(!raw);
        assert!(!alternate);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_onboarding_api_key_shutdown_starts_no_save() {
        let saved = Cell::new(false);
        let result = save_api_key_after_observation(
            || true,
            || {
                saved.set(true);
                Ok::<(), io::Error>(())
            },
        );
        assert!(matches!(result, Err(OnboardingError::Cancelled)));
        assert!(!saved.get());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_onboarding_oauth_return_after_shutdown_does_not_reenter_raw() {
        let observations = Cell::new(0);
        let entered_raw = Cell::new(false);
        let successful_login = reenter_after_oauth_success(
            || true,
            || {
                entered_raw.set(true);
                Ok(())
            },
        );
        assert!(matches!(successful_login, Ok(None)));
        assert!(!entered_raw.get());

        let result = reenter_after_oauth_error(
            "oauth exchange failed",
            || {
                let next = observations.get() + 1;
                observations.set(next);
                next == 2
            },
            || {
                entered_raw.set(true);
                Ok(())
            },
        );
        assert_eq!(observations.get(), 2);
        assert!(!entered_raw.get());
        assert!(matches!(
            result,
            Err(OnboardingError::Auth(message)) if message == "oauth exchange failed"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signal_onboarding_completed_pre_observation_save_is_preserved() {
        let pending = Cell::new(false);
        let saved = Cell::new(0);
        let result = save_api_key_after_observation(
            || pending.get(),
            || {
                saved.set(saved.get() + 1);
                pending.set(true);
                Ok::<(), io::Error>(())
            },
        );
        assert_eq!(saved.get(), 1);
        assert!(pending.get());
        assert!(matches!(result, Ok(Outcome::ApiKeySaved)));
    }

    /// Delegates to `TestBackend` and keeps every flushed screen, so a
    /// sentinel typed into the masked prompt can be checked against all of them.
    struct Recording {
        inner: TestBackend,
        frames: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl ratatui::backend::Backend for Recording {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.draw(content)
        }
        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            self.inner.set_cursor_position(position)
        }
        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.clear()
        }
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.size()
        }
        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size()
        }
        fn scroll_region_up(
            &mut self,
            region: std::ops::Range<u16>,
            line_count: u16,
        ) -> std::io::Result<()> {
            self.inner.scroll_region_up(region, line_count)
        }
        fn scroll_region_down(
            &mut self,
            region: std::ops::Range<u16>,
            line_count: u16,
        ) -> std::io::Result<()> {
            self.inner.scroll_region_down(region, line_count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let screen = self
                .inner
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            self.frames.borrow_mut().push(screen);
            self.inner.flush()
        }
    }

    fn drive(setup: &DeciderSetup, keys: &[KeyCode]) -> (DeciderResult, Vec<String>) {
        use ratatui::crossterm::event::Event;
        let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut terminal = Terminal::new(Recording {
            inner: TestBackend::new(110, 20),
            frames: frames.clone(),
        })
        .expect("terminal");
        let mut queue: std::collections::VecDeque<_> = keys.iter().copied().collect();
        let result = decider_step(
            &mut terminal,
            setup,
            || queue.pop_front().map(|code| Event::Key(press(code))),
            true,
        );
        let frames = frames.borrow().clone();
        (result, frames)
    }

    fn setup(dir: &std::path::Path, local_only: bool) -> DeciderSetup {
        DeciderSetup {
            key_path: dir.join("decision_keys.json"),
            config_path: dir.join("config.toml"),
            local_only,
        }
    }

    #[test]
    fn the_decider_screen_offers_jev_with_its_disclosure() {
        let dir = tempfile::tempdir().unwrap();
        let (result, frames) = drive(&setup(dir.path(), false), &[KeyCode::Esc]);
        assert_eq!(result, DeciderResult::Skipped);
        assert!(frames[0].contains("1. Jev"));
        assert!(frames[0].contains("api.typesafe.ai"));
        assert!(!dir.path().join("decision_keys.json").exists());
        assert!(!dir.path().join("config.toml").exists());
    }

    #[test]
    fn no_decider_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (result, _) = drive(&setup(dir.path(), false), &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(result, DeciderResult::Skipped);
        assert!(!dir.path().join("decision_keys.json").exists());
        assert!(!dir.path().join("config.toml").exists());
    }

    #[test]
    fn local_only_hides_jev_and_enter_continues_without_saving() {
        let dir = tempfile::tempdir().unwrap();
        let (result, frames) = drive(&setup(dir.path(), true), &[KeyCode::Up, KeyCode::Enter]);
        assert_eq!(result, DeciderResult::Skipped);
        assert!(!frames[0].contains("1. Jev"));
        assert!(frames[0].contains("local_only"));
        assert!(!dir.path().join("decision_keys.json").exists());
    }

    #[test]
    fn a_typed_jev_key_is_saved_enabled_and_never_drawn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[embedding]\n").unwrap();
        let sentinel = "jevsentinel0123";
        let mut keys = vec![KeyCode::Enter];
        keys.extend(sentinel.chars().map(KeyCode::Char));
        keys.extend([KeyCode::Enter, KeyCode::Char('x')]);
        let (result, frames) = drive(&setup(dir.path(), false), &keys);
        assert_eq!(result, DeciderResult::Enabled);
        assert!(frames.iter().any(|frame| frame.contains("Jev API key")));
        assert!(frames.iter().any(|frame| frame.contains("Jev is enabled")));
        assert!(frames.iter().all(|frame| !frame.contains(sentinel)));
        // Contrast: the key did reach the owner-only store.
        assert!(
            std::fs::read_to_string(dir.path().join("decision_keys.json"))
                .unwrap()
                .contains(sentinel)
        );
        let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(config.starts_with("[embedding]\n"));
        assert!(config.contains("[decision]") && !config.contains(sentinel));
    }

    #[test]
    fn a_pasted_jev_key_is_accepted_as_a_whole() {
        use ratatui::crossterm::event::Event;
        let dir = tempfile::tempdir().unwrap();
        let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut terminal = Terminal::new(Recording {
            inner: TestBackend::new(110, 20),
            frames: frames.clone(),
        })
        .unwrap();
        let mut events = std::collections::VecDeque::from([
            Event::Key(press(KeyCode::Enter)),
            Event::Paste("  pastedsentinel42  ".into()),
            Event::Key(press(KeyCode::Enter)),
            Event::Key(press(KeyCode::Char('x'))),
        ]);
        let result = decider_step(
            &mut terminal,
            &setup(dir.path(), false),
            || events.pop_front(),
            false,
        );
        assert_eq!(result, DeciderResult::Enabled);
        let stored = std::fs::read_to_string(dir.path().join("decision_keys.json")).unwrap();
        assert!(stored.contains("\"pastedsentinel42\""));
        assert!(
            frames
                .borrow()
                .iter()
                .any(|frame| frame.contains("next launch"))
        );
        assert!(
            frames
                .borrow()
                .iter()
                .all(|frame| !frame.contains("pastedsentinel42"))
        );
    }

    #[test]
    fn number_keys_select_instead_of_flipping() {
        let dir = tempfile::tempdir().unwrap();
        // `2` selects "No decider" and Enter then saves nothing; `1` back to Jev.
        let (result, _) = drive(
            &setup(dir.path(), false),
            &[KeyCode::Char('2'), KeyCode::Enter],
        );
        assert_eq!(result, DeciderResult::Skipped);
        let keys = [
            KeyCode::Char('2'),
            KeyCode::Char('1'),
            KeyCode::Enter,
            KeyCode::Char('k'),
            KeyCode::Enter,
            KeyCode::Char('x'),
        ];
        let (result, _) = drive(&setup(dir.path(), false), &keys);
        assert_eq!(result, DeciderResult::Enabled);
    }

    #[test]
    fn the_disclosure_and_prompt_survive_an_80_column_terminal() {
        use ratatui::crossterm::event::Event;
        let dir = tempfile::tempdir().unwrap();
        let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut terminal = Terminal::new(Recording {
            inner: TestBackend::new(80, 24),
            frames: frames.clone(),
        })
        .unwrap();
        let mut events = std::collections::VecDeque::from([
            Event::Key(press(KeyCode::Enter)),
            Event::Key(press(KeyCode::Esc)),
        ]);
        decider_step(
            &mut terminal,
            &setup(dir.path(), false),
            || events.pop_front(),
            false,
        );
        let frames = frames.borrow();
        // Wrapping splits a sentence across rows and the border draws between
        // them, so compare only the ASCII non-space characters.
        let squash = |text: &str| {
            text.chars()
                .filter(|c| c.is_ascii() && !c.is_whitespace())
                .collect::<String>()
        };
        assert!(squash(&frames[0]).contains(&squash("to classify each turn.")));
        assert!(squash(&frames[1]).contains("Esctoskip."));
    }

    #[test]
    fn a_pasted_key_with_a_control_character_is_ignored() {
        use ratatui::crossterm::event::Event;
        let dir = tempfile::tempdir().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(110, 20)).unwrap();
        let mut events = std::collections::VecDeque::from([
            Event::Key(press(KeyCode::Enter)),
            Event::Paste("bad\nkey".into()),
            Event::Key(press(KeyCode::Enter)),
            Event::Key(press(KeyCode::Esc)),
        ]);
        let result = decider_step(
            &mut terminal,
            &setup(dir.path(), false),
            || events.pop_front(),
            false,
        );
        assert_eq!(result, DeciderResult::Skipped);
        assert!(!dir.path().join("decision_keys.json").exists());
    }

    #[test]
    fn esc_in_the_key_prompt_skips_without_saving() {
        let dir = tempfile::tempdir().unwrap();
        let (result, _) = drive(
            &setup(dir.path(), false),
            &[KeyCode::Enter, KeyCode::Char('a'), KeyCode::Esc],
        );
        assert_eq!(result, DeciderResult::Skipped);
        assert!(!dir.path().join("decision_keys.json").exists());
    }

    #[test]
    fn an_existing_decision_setting_keeps_the_config_and_still_saves_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let original = "[decision]\nenabled = false\n";
        std::fs::write(dir.path().join("config.toml"), original).unwrap();
        let keys = [
            KeyCode::Enter,
            KeyCode::Char('k'),
            KeyCode::Enter,
            KeyCode::Char('x'),
        ];
        let (result, _) = drive(&setup(dir.path(), false), &keys);
        assert!(matches!(result, DeciderResult::SavedOnly(_)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            original
        );
        assert!(dir.path().join("decision_keys.json").exists());
    }

    #[test]
    fn the_choice_screen_marks_the_selected_option() {
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_choice_screen(f, Choice::ChatGpt))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("Welcome to polaris"));
        assert!(content.contains("Sign in with ChatGPT"));
        assert!(content.contains("Provide your own API key"));
        assert!(content.contains("Pay for what you use"));
    }

    #[test]
    fn the_choice_screen_highlights_whichever_option_is_selected() {
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_choice_screen(f, Choice::ApiKey))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let find_row = |needle: &str| {
            for y in 0..buffer.area.height {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if let Some(x) = row.find(needle) {
                    return Some(buffer[(x as u16, y)].fg);
                }
            }
            None
        };
        assert_eq!(
            find_row("2. Provide your own API key"),
            Some(Color::Cyan),
            "the selected option's marker column should be highlighted"
        );
        assert_ne!(
            find_row("1. Sign in with ChatGPT"),
            Some(Color::Cyan),
            "the unselected option should not be highlighted"
        );
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

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("*****"));
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn up_down_and_digit_keys_all_toggle_the_choice() {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('1'),
            KeyCode::Char('2'),
        ] {
            assert!(matches!(
                apply_choice_key(press(code)),
                ChoiceAction::Toggle
            ));
        }
    }

    #[test]
    fn enter_submits_the_choice_screen() {
        assert!(matches!(
            apply_choice_key(press(KeyCode::Enter)),
            ChoiceAction::Submit
        ));
    }

    #[test]
    fn escape_and_ctrl_c_quit_the_choice_screen() {
        for key in [
            press(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(apply_choice_key(key), ChoiceAction::Quit));
        }
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
    fn escape_and_ctrl_c_quit_the_key_entry_screen() {
        for key in [
            press(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut buffer = "partial".to_string();
            assert!(matches!(
                apply_key_entry_key(&mut buffer, key),
                KeyEntryAction::Quit
            ));
        }
    }

    #[tokio::test]
    async fn a_non_interactive_terminal_is_refused_immediately() {
        // Cargo may inherit a real terminal. Run this test in a child with
        // explicit non-terminal streams, never against the user's TTY.
        const CHILD: &str = "POLARIS_TEST_NON_INTERACTIVE_ONBOARDING";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "onboarding::tests::a_non_interactive_terminal_is_refused_immediately",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .expect("run non-interactive child");
            assert!(
                output.status.success(),
                "child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert!(!std::io::stdin().is_terminal());
        assert!(!std::io::stdout().is_terminal());
        let dir = tempfile::tempdir().expect("temp dir");
        let auth_path = dir.path().join("auth.json");
        let key_path = dir.path().join("api_key.json");

        let decider = setup(dir.path(), false);
        let err = run(&auth_path, &key_path, &decider)
            .await
            .expect_err("a non-interactive terminal must be refused");
        assert!(matches!(err, OnboardingError::NonInteractive));
        assert!(!auth_path.exists());
        assert!(!key_path.exists());
    }
}
