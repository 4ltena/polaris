//! Provider-free Windows browsing while native mutation owners are unavailable.

use super::*;
use super::{LinuxTerminalGuard as TerminalGuard, LinuxTerminalStep as TerminalStep};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, IsTerminal};

#[cfg(test)]
mod tests;

pub fn run_windows_read_only() -> ExitCode {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("Can't start the Windows read-only TUI on a non-interactive terminal");
        return ExitCode::FAILURE;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("Can't determine the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let initialized = enter_terminal_with(
        |step| match step {
            TerminalStep::Alternate => ratatui::crossterm::execute!(
                io::stdout(),
                ratatui::crossterm::terminal::EnterAlternateScreen
            ),
            TerminalStep::Raw => ratatui::crossterm::terminal::enable_raw_mode(),
            _ => unreachable!("only startup steps are requested"),
        },
        || {
            ratatui::Terminal::with_options(
                crate::terminal::backend(),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fullscreen,
                },
            )
        },
        linux_restore_terminal_step,
    );
    let (mut guard, mut terminal) = match initialized {
        Ok(initialized) => initialized,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    guard.enable_mouse_and_cursor();
    let mut view = ReadOnlyView::new(cwd);
    let result = run_loop(&mut terminal, &mut view, crate::terminal::read_event);
    match linux_finish_guarded_terminal(result, &mut guard, linux_restore_terminal_step) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn enter_terminal_with<T>(
    mut acquire: impl FnMut(TerminalStep) -> io::Result<()>,
    make_terminal: impl FnOnce() -> io::Result<T>,
    mut restore: impl FnMut(TerminalStep) -> io::Result<()>,
) -> Result<(TerminalGuard, T), String> {
    let mut guard = TerminalGuard::default();
    for step in [TerminalStep::Alternate, TerminalStep::Raw] {
        if let Err(error) = acquire(step) {
            return linux_finish_guarded_terminal(
                Err(format!("Can't initialize terminal ({step:?}): {error}")),
                &mut guard,
                &mut restore,
            );
        }
        match step {
            TerminalStep::Alternate => guard.alternate = true,
            TerminalStep::Raw => guard.raw = true,
            _ => unreachable!("fixed startup steps"),
        }
    }
    match make_terminal() {
        Ok(terminal) => Ok((guard, terminal)),
        Err(error) => linux_finish_guarded_terminal(
            Err(format!("Can't initialize terminal: {error}")),
            &mut guard,
            &mut restore,
        ),
    }
}

struct ReadOnlyView {
    cwd: PathBuf,
    lines: Vec<render::HistoryLine>,
    wrap_cache: render::HistoryWrapCache,
    buffer: String,
    cursor: usize,
    history: input::History,
    status: Status,
    scroll: usize,
    selected: usize,
    exit: ExitConfirmation,
}

impl ReadOnlyView {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            lines: Vec::new(),
            wrap_cache: render::HistoryWrapCache::default(),
            buffer: String::new(),
            cursor: 0,
            history: input::History::new(),
            status: Status::Notice(
                "Browsing only. Use /help to inspect available operations".into(),
            ),
            scroll: 0,
            selected: 0,
            exit: ExitConfirmation { armed: false },
        }
    }

    fn suggestions(&self) -> Vec<&'static slash::SlashCommand> {
        self.buffer
            .strip_prefix('/')
            .map_or_else(Vec::new, |prefix| {
                slash::matching(prefix)
                    .into_iter()
                    .filter(|command| supported(&slash::action_for(command.name)))
                    .map(read_only_command)
                    .collect()
            })
    }

    fn draw<B: Backend>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<(), String> {
        let cwd = display_text(&self.cwd.display().to_string());
        let header = render::HeaderInfo {
            provider_name: "disconnected",
            model_name: "not selected",
            effort_name: "",
            usage: polaris_provider::Usage::default(),
            cwd: &cwd,
            cwd_short: &cwd,
        };
        let suggestions = self.suggestions();
        terminal
            .draw(|frame| {
                let mut history =
                    render::header_history_lines_with_mode(&header, frame.area().width, true);
                history.extend(self.lines.iter().cloned());
                draw_frame_with_mode(
                    frame,
                    &history,
                    &mut self.wrap_cache,
                    self.scroll,
                    &self.buffer,
                    self.cursor,
                    &self.status,
                    &header,
                    &suggestions,
                    self.selected,
                    &None,
                    true,
                );
            })
            .map(|_| ())
            .map_err(|error| format!("Can't draw the read-only TUI: {error}"))
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.key(key),
            Event::Paste(mut text) => {
                self.exit.cancel();
                self.buffer.insert_str(self.cursor, &text);
                self.cursor += text.len();
                text.zeroize();
                self.selected = 0;
                if redact_inline_secret_value(&mut self.buffer, &mut self.cursor, &mut self.history)
                {
                    self.status =
                        Status::Error("Secrets cannot be entered in the normal input field".into());
                }
                false
            }
            Event::Resize(_, _) => {
                self.wrap_cache.invalidate();
                false
            }
            _ => false,
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.exit.press() {
                return true;
            }
            self.status = Status::Notice("Press Ctrl+C again to exit".into());
            return false;
        }
        self.exit.cancel();
        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.status = Status::Error(WINDOWS_OWNER_UNAVAILABLE.into());
            return false;
        }
        if secret_value_key_is_blocked(&self.buffer, &key) {
            self.status =
                Status::Error("Secrets cannot be entered in the normal input field".into());
            return false;
        }
        if key.code == KeyCode::Enter && is_secret_command(&self.buffer) {
            self.buffer.zeroize();
            self.cursor = 0;
            self.history.clear_draft();
            self.status = Status::Error("This TUI does not accept secret configuration".into());
            return false;
        }
        let suggestions = self.suggestions();
        match key.code {
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up | KeyCode::Down if !suggestions.is_empty() => {
                self.selected = if key.code == KeyCode::Down {
                    (self.selected + 1) % suggestions.len()
                } else {
                    (self.selected + suggestions.len() - 1) % suggestions.len()
                };
            }
            KeyCode::Up | KeyCode::Down => {
                let recalled = if key.code == KeyCode::Up {
                    self.history.older(&self.buffer)
                } else {
                    self.history.newer()
                };
                if let Some(text) = recalled {
                    self.buffer = text.to_owned();
                    self.cursor = self.buffer.len();
                }
            }
            KeyCode::Enter if !suggestions.is_empty() => {
                let command = suggestions[self.selected.min(suggestions.len() - 1)].name;
                return self.submit(format!("/{command}"), slash::action_for(command));
            }
            _ => {
                let original_cursor = self.cursor;
                match apply_admitted_key(&mut self.buffer, &mut self.cursor, key) {
                    Ok(InputAction::Submit(text)) if text.trim().is_empty() => {}
                    Ok(InputAction::Submit(text)) => {
                        // The dispatch may need an unavailable private reader even
                        // for a read-only action. Keep its original draft on error.
                        self.buffer = text.clone();
                        self.cursor = original_cursor;
                        if let Some(action) = slash::parse(&text) {
                            return self.submit(text, action);
                        }
                        self.status = Status::Error(WINDOWS_OWNER_UNAVAILABLE.into());
                    }
                    Ok(_) => {
                        self.selected = 0;
                        if redact_inline_secret_value(
                            &mut self.buffer,
                            &mut self.cursor,
                            &mut self.history,
                        ) {
                            self.status = Status::Error(
                                "Secrets cannot be entered in the normal input field".into(),
                            );
                        }
                    }
                    Err(error) => self.status = Status::Error(error.into()),
                }
            }
        }
        false
    }

    fn submit(&mut self, text: String, action: slash::Action) -> bool {
        if let Err(error) = require_action_owner(&action) {
            self.status = Status::Error(error.into());
            return false;
        }
        if matches!(action, slash::Action::Quit) {
            if self.history.has_hidden_draft() {
                self.status = Status::Error(
                    "A draft is behind the history view. Restore it before exiting".into(),
                );
                return false;
            }
            return true;
        }
        match command_lines(&action, &self.cwd) {
            Ok(lines) => {
                self.history.record(&text);
                self.buffer.clear();
                self.cursor = 0;
                self.selected = 0;
                self.scroll = 0;
                self.lines.push(render::HistoryLine {
                    line: ratatui::text::Line::from(display_text(&text)),
                    shaded: true,
                });
                self.lines
                    .extend(lines.into_iter().map(|line| render::HistoryLine {
                        line: ratatui::text::Line::from(display_text(&line)),
                        shaded: false,
                    }));
                self.wrap_cache.invalidate();
                self.status =
                    Status::Notice("Browsing result displayed. Nothing was saved or sent".into());
            }
            Err(error) => self.status = Status::Error(error),
        }
        false
    }
}

fn supported(action: &slash::Action) -> bool {
    matches!(
        action,
        slash::Action::Help
            | slash::Action::Status
            | slash::Action::Doctor
            | slash::Action::Pwd
            | slash::Action::Compare(_)
            | slash::Action::Quit
            | slash::Action::Phase(_)
            | slash::Action::Workflow(_)
            | slash::Action::Decided(_)
    ) && action.allowed_without_mutation_owner()
}

fn read_only_command(command: &'static slash::SlashCommand) -> &'static slash::SlashCommand {
    const PHASE: slash::SlashCommand = slash::SlashCommand {
        name: "phase",
        description: "show phase availability (read-only)",
    };
    const WORKFLOW: slash::SlashCommand = slash::SlashCommand {
        name: "workflow",
        description: "show workflow availability (read-only)",
    };
    match command.name {
        "phase" => &PHASE,
        "workflow" => &WORKFLOW,
        _ => command,
    }
}

fn command_lines(action: &slash::Action, cwd: &Path) -> Result<Vec<String>, String> {
    let lines = match action {
        slash::Action::Help => slash::COMMANDS
            .iter()
            .filter(|command| supported(&slash::action_for(command.name)))
            .map(read_only_command)
            .map(|command| format!("/{} — {}", command.name, command.description))
            .collect(),
        slash::Action::Status | slash::Action::Doctor => vec![
            "Windows browsing only".into(),
            "Model connection, conversation, storage: not started".into(),
            "Authentication, MCP, saved sessions: not loaded (Windows protection mechanisms unsupported)".into(),
            "Normal input, changes, subprocess execution: denied as unsupported".into(),
            "This screen does not save, send, or run external commands".into(),
        ],
        slash::Action::Pwd => vec![
            polaris_core::project::resolve_root(cwd)
                .display()
                .to_string(),
        ],
        slash::Action::Decided(value) if matches!(value.as_str(), "" | "list") => {
            return decided_read_lines(cwd)
                .map(|lines| lines.iter().map(ToString::to_string).collect());
        }
        slash::Action::Compare(value) => {
            return compare_read_lines(cwd, value)
                .map(|lines| lines.iter().map(ToString::to_string).collect());
        }
        slash::Action::Phase(value) | slash::Action::Workflow(value) if value.is_empty() => {
            vec!["No conversation started. Saved workflow is not loaded".into()]
        }
        _ => return Err(WINDOWS_OWNER_UNAVAILABLE.into()),
    };
    Ok(lines)
}

fn display_text(text: &str) -> String {
    render::sanitize(text)
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn run_loop<B: Backend>(
    terminal: &mut ratatui::Terminal<B>,
    view: &mut ReadOnlyView,
    read: impl FnMut() -> io::Result<Event>,
) -> Result<(), String> {
    run_loop_with_draw(view, |view| view.draw(terminal), read)
}

fn run_loop_with_draw(
    view: &mut ReadOnlyView,
    mut draw: impl FnMut(&mut ReadOnlyView) -> Result<(), String>,
    mut read: impl FnMut() -> io::Result<Event>,
) -> Result<(), String> {
    loop {
        draw(view)?;
        let event = read().map_err(|error| format!("Can't read terminal input: {error}"))?;
        if view.event(event) {
            return Ok(());
        }
    }
}
