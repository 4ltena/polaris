//! Pure rendering: turns a `Session` + input state into terminal cells.
//! Kept free of any actual terminal I/O so it's testable with
//! `ratatui::backend::TestBackend`.

use std::time::Duration;

use polaris_core::{AgentEvent, DiffLine};
use polaris_provider::Role;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

/// What the bottom-of-screen status line shows while a turn is in flight,
/// idle, or after a failed turn.
pub enum Status {
    Idle,
    /// A turn is in flight. `elapsed` is how long it's been running —
    /// the caller re-sets this on every redraw tick (see `lib.rs`'s
    /// `tokio::select!` around the agent turn) so the shimmer sweep and
    /// the `Ns` counter both animate live, the way codex's own status
    /// row does (verified against its `status_indicator_widget.rs` and
    /// `shimmer.rs`).
    Thinking {
        elapsed: Duration,
    },
    Error(String),
    /// The local, non-error result of a slash command (`/status`,
    /// `/help`, an unrecognized `/foo`). Shown the same way `Error` is,
    /// but without the "error:" framing — it isn't one.
    Notice(String),
}

/// A moving highlight band sweeping across `text`, timed by `elapsed`
/// rather than wall-clock/process-start the way codex's `shimmer.rs`
/// does — `render.rs` stays pure/testable, so the caller supplies the
/// clock instead of this reading one itself. Same shape otherwise: a
/// 2-second sweep period, a cosine falloff band five characters wide on
/// each side of the sweep position, blended between a dim base color and
/// a bright highlight (codex blends the terminal's actual detected
/// fg/bg; a fixed gray-to-white blend is close enough here without
/// pulling in terminal-palette detection for one animation).
fn shimmer_spans(text: &str, elapsed: Duration) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let padding = 10isize;
    let period = chars.len() as isize + padding * 2;
    let sweep_seconds = 2.0f32;
    let pos = ((elapsed.as_secs_f32() % sweep_seconds) / sweep_seconds * period as f32) as isize;
    let band_half_width = 5.0f32;
    const BASE: (f32, f32, f32) = (110.0, 110.0, 110.0);
    const HIGHLIGHT: (f32, f32, f32) = (255.0, 255.0, 255.0);

    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let dist = ((i as isize + padding) - pos).abs() as f32;
            let t = if dist <= band_half_width {
                let x = std::f32::consts::PI * (dist / band_half_width);
                0.5 * (1.0 + x.cos())
            } else {
                0.0
            };
            let r = (BASE.0 + (HIGHLIGHT.0 - BASE.0) * t) as u8;
            let g = (BASE.1 + (HIGHLIGHT.1 - BASE.1) * t) as u8;
            let b = (BASE.2 + (HIGHLIGHT.2 - BASE.2) * t) as u8;
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// Replace ASCII control characters that could smuggle terminal escape /
/// OSC sequences (most importantly ESC, 0x1B) into the real terminal once
/// this renders through crossterm. `\n` and `\t` are left alone since they
/// don't start escape sequences and are needed for readable text; every
/// other C0 control character (0x00-0x1F) and DEL (0x7F) is replaced with
/// the Unicode replacement character so the presence of hidden bytes is
/// still visible rather than silently dropped.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if (c as u32) < 0x20 || c as u32 == 0x7f => '\u{fffd}',
            c => c,
        })
        .collect()
}

fn label(role: Role) -> &'static str {
    match role {
        Role::User => "you",
        Role::Assistant => "polaris",
        Role::Tool => "tool",
    }
}

const TOOL_RESULT_PREVIEW_CHARS: usize = 200;

/// The most diff lines (context/added/removed, combined across all hunks)
/// shown live for one `ToolFinished { diff: Some(_), .. }` event, before the
/// rest is elided with a notice row — the live-print analog of
/// `TOOL_RESULT_PREVIEW_CHARS`, bounding how much screen a single write/edit
/// can claim mid-turn.
const MAX_DIFF_LINES_SHOWN: usize = 40;

/// How many rows one spawned subagent's task preview may claim at most,
/// before the rest is elided with a `...` row.
const SPAWN_TASK_PREVIEW_LINES: usize = 5;

/// A tool result's live preview, already sanitized and split into rows.
/// Empty when the result carries nothing worth a row (an empty body, or a
/// body of nothing but whitespace).
///
/// The truncation matches the batch-print era's `format_tool_result`: tabs
/// expanded first so the budget is spent on what is actually rendered, then
/// `TOOL_RESULT_PREVIEW_CHARS` characters, then `...` if anything was cut.
/// Rows are capped at `SPAWN_TASK_PREVIEW_LINES` for the same reason they
/// are there — the character budget alone still lets 200 newlines through.
fn format_tool_result_preview(result: &str) -> Vec<String> {
    if result.trim().is_empty() {
        return Vec::new();
    }
    let expanded = result.replace('\t', "    ");
    let preview: String = expanded.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
    let body = if expanded.chars().count() > TOOL_RESULT_PREVIEW_CHARS {
        format!("  → {preview}...")
    } else {
        format!("  → {preview}")
    };
    let sanitized = sanitize(&body);
    let all: Vec<&str> = sanitized.split('\n').collect();
    let shown = all.len().min(SPAWN_TASK_PREVIEW_LINES);
    let mut rows: Vec<String> = all[..shown]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                (*line).to_string()
            } else {
                format!("    {line}")
            }
        })
        .collect();
    if all.len() > shown {
        rows.push("    ...".to_string());
    }
    rows
}

/// Turns an `AgentEvent` into lines printable the moment it arrives,
/// mid-turn. A write/edit `ToolFinished` carrying a `diff` renders its
/// added/removed lines (capped at `MAX_DIFF_LINES_SHOWN`, combined across
/// all hunks) right below the done/failed marker; every other event covers
/// only the start/finish markers for ordinary tool calls and for spawned
/// subagents.
pub fn format_event_for_live_print(event: &AgentEvent) -> Vec<HistoryLine> {
    match event {
        AgentEvent::ToolStarted { name, detail } => {
            let preview: String = detail.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            vec![HistoryLine::plain(Line::from(Span::raw(sanitize(
                &format!("⏺ {name}({preview})"),
            ))))]
        }
        AgentEvent::ToolFinished {
            ok, result, diff, ..
        } => {
            let marker = if *ok { "done" } else { "failed" };
            let mut lines = vec![HistoryLine::plain(Line::from(Span::styled(
                format!("  {marker}"),
                Style::default().add_modifier(Modifier::DIM),
            )))];
            // 結果本文の短いプレビュー。切り詰め方はバッチ表示時代の
            // `format_tool_result`と同じ——タブを展開してから
            // `TOOL_RESULT_PREVIEW_CHARS`文字で切り、続きがあれば`...`。
            // `sanitize`は`\n`を残すので、1行=1行分の枠しかない
            // `HistoryLine`に流し込む前に行へ割る。
            for row in format_tool_result_preview(result) {
                lines.push(HistoryLine::plain(Line::from(Span::styled(
                    row,
                    Style::default().add_modifier(Modifier::DIM),
                ))));
            }
            if let Some(d) = diff {
                let kind = if d.is_new_file { "Created" } else { "Updated" };
                lines.push(HistoryLine::plain(Line::from(Span::styled(
                    sanitize(&format!(
                        "  {kind} — Added {} lines, removed {} lines",
                        d.added, d.removed
                    )),
                    Style::default().add_modifier(Modifier::DIM),
                ))));
                let mut shown = 0usize;
                'hunks: for hunk in &d.hunks {
                    for dl in &hunk.lines {
                        if shown >= MAX_DIFF_LINES_SHOWN {
                            lines.push(HistoryLine::plain(Line::from(Span::styled(
                                "  ...(省略)",
                                Style::default().add_modifier(Modifier::DIM),
                            ))));
                            break 'hunks;
                        }
                        let (prefix, style) = match dl {
                            DiffLine::Context(_) => {
                                ("  ", Style::default().add_modifier(Modifier::DIM))
                            }
                            DiffLine::Added(_) => ("+ ", Style::default().fg(Color::Green)),
                            DiffLine::Removed(_) => ("- ", Style::default().fg(Color::Red)),
                        };
                        let text = match dl {
                            DiffLine::Context(s) | DiffLine::Added(s) | DiffLine::Removed(s) => s,
                        };
                        lines.push(HistoryLine::plain(Line::from(Span::styled(
                            sanitize(&format!("{prefix}{text}")),
                            style,
                        ))));
                        shown += 1;
                    }
                }
            }
            lines
        }
        AgentEvent::SpawnStarted { agent_type, task } => {
            // `task` is free-form, model-supplied prose (`spawn`'s
            // `tasks[].task` argument) and is routinely multi-line, unlike
            // a tool call's `detail`, which is compact JSON and so has its
            // newlines escaped. `sanitize` deliberately keeps a real `\n`,
            // and `render_history_into` reserves exactly one terminal row
            // per `HistoryLine`, so a newline left inside one line would be
            // written into the middle of a single-row slot. Split it.
            //
            // Truncated to the same preview budget as everything else here
            // *before* splitting, which also bounds the row count; the
            // explicit line cap then covers the pathological all-newlines
            // case that the character budget alone would still let through.
            let preview: String = task.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            let sanitized = sanitize(&format!("⏺ Agent({agent_type}: {preview})"));
            let all: Vec<&str> = sanitized.split('\n').collect();
            let shown = all.len().min(SPAWN_TASK_PREVIEW_LINES);
            let mut lines: Vec<HistoryLine> = all[..shown]
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    // Continuation lines are indented under the first,
                    // which carries the `⏺ Agent(...)` prefix.
                    let text = if i == 0 {
                        (*line).to_string()
                    } else {
                        format!("    {line}")
                    };
                    HistoryLine::plain(Line::from(Span::raw(text)))
                })
                .collect();
            if all.len() > shown {
                lines.push(HistoryLine::plain(Line::from(Span::raw("    ..."))));
            }
            lines.push(HistoryLine::plain(Line::from(Span::styled(
                "  Backgrounded agent",
                Style::default().add_modifier(Modifier::DIM),
            ))));
            lines
        }
        AgentEvent::SpawnFinished { agent_type, ok } => {
            let marker = if *ok { "done" } else { "failed" };
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!("  {agent_type}: {marker}")),
                Style::default().add_modifier(Modifier::DIM),
            )))]
        }
    }
}

fn role_color(role: Role) -> Color {
    match role {
        // The user's own input is distinguished by its full-row gray
        // background (`HistoryLine::shaded`), not by a tinted foreground —
        // a colored fg on top of the shading reads as two competing cues
        // for the same thing. The assistant's replies stay on the
        // terminal's own default foreground too, matching plain
        // conversational text rather than a status/log color.
        Role::User | Role::Assistant => Color::Reset,
        Role::Tool => Color::Yellow,
    }
}

/// Turns one line of already-sanitized text into styled spans, recognizing
/// `**bold**` and `` `inline code` ``. Malformed markdown (an unclosed `**`
/// or `` ` ``) is not an error — whatever's left over after the last
/// successfully matched marker is emitted as plain text, so a stray
/// backtick never breaks rendering.
fn format_inline(text: &str, base_color: Color) -> Vec<Span<'static>> {
    let base_style = Style::default().fg(base_color);
    let mut spans = Vec::new();
    let mut rest = text;

    loop {
        // Find whichever marker comes first: **bold** or `code`.
        let bold_pos = rest.find("**");
        let code_pos = rest.find('`');

        // Whichever marker starts earlier goes first; a tie or a marker
        // with no competitor also counts as "first". `bold_first` is only
        // consulted once we know at least one marker exists (the `(None,
        // None)` case above already broke out of the loop), so `.unwrap()`
        // below on the corresponding position is always safe.
        let bold_first = match (bold_pos, code_pos) {
            (None, None) => {
                if !rest.is_empty() {
                    spans.push(Span::styled(rest.to_string(), base_style));
                }
                break;
            }
            (Some(b), Some(c)) => b <= c,
            (Some(_), None) => true,
            (None, Some(_)) => false,
        };

        if bold_first {
            let start = bold_pos.unwrap();
            if let Some(end) = rest[start + 2..].find("**") {
                let end = start + 2 + end;
                if start > 0 {
                    spans.push(Span::styled(rest[..start].to_string(), base_style));
                }
                spans.push(Span::styled(
                    rest[start + 2..end].to_string(),
                    base_style.add_modifier(Modifier::BOLD),
                ));
                rest = &rest[end + 2..];
            } else {
                // Unclosed `**`: emit the rest as plain text.
                spans.push(Span::styled(rest.to_string(), base_style));
                break;
            }
        } else {
            let start = code_pos.unwrap();
            if let Some(end) = rest[start + 1..].find('`') {
                let end = start + 1 + end;
                if start > 0 {
                    spans.push(Span::styled(rest[..start].to_string(), base_style));
                }
                spans.push(Span::styled(
                    rest[start + 1..end].to_string(),
                    base_style.bg(Color::DarkGray),
                ));
                rest = &rest[end + 1..];
            } else {
                // Unclosed backtick: emit the rest as plain text.
                spans.push(Span::styled(rest.to_string(), base_style));
                break;
            }
        }
    }

    spans
}

/// One already-formatted, sanitized line of conversation history, plus
/// whether it should render with a full-row gray background. `shaded` is
/// true only for a user's own input, once it's scrolled into history —
/// matching codex's own styling (verified against a real screenshot: the
/// user's echoed input line sits on a gray block, every other line —
/// assistant output, tool activity — stays on the terminal's default
/// background). A `Paragraph`'s own background fill only covers the exact
/// character cells it draws text into, not a whole row, so shading a full
/// row needs one row-height `Paragraph` per `HistoryLine` rather than one
/// `Paragraph` for a whole block of lines — see `render_history_into`.
pub struct HistoryLine {
    pub line: Line<'static>,
    pub shaded: bool,
}

impl HistoryLine {
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            shaded: false,
        }
    }
}

/// Formats a slice of already-existing messages into printable lines. Takes
/// a slice (not the whole `Session`) so a caller can format only the
/// messages that haven't been printed to the real terminal yet — the
/// inline-viewport model prints each new turn once and never redraws it,
/// unlike the old full-history-every-frame approach this replaces.
///
/// Deliberately covers *only* the user's and the assistant's own text.
/// Tool-call activity (`Message::tool_calls`, `Role::Tool` results) is
/// printed live, while the turn is still running, from the `AgentEvent`
/// stream via `format_event_for_live_print` — formatting it here too
/// would print every tool call a second time when the finished turn's
/// messages are batch-printed.
pub fn history_lines_for(messages: &[polaris_provider::Message]) -> Vec<HistoryLine> {
    messages
        .iter()
        .flat_map(|m| -> Vec<HistoryLine> {
            let color = role_color(m.role);
            let shaded = matches!(m.role, Role::User);
            match m.role {
                Role::Tool => Vec::new(),
                Role::User | Role::Assistant => {
                    let mut lines = Vec::new();
                    if !m.content.is_empty() {
                        let sanitized = sanitize(&m.content);
                        // The user's own line needs no "you:" label — its
                        // full-row gray shading already marks it as input,
                        // the same way codex's own screen has no label on
                        // the user's echoed line either. The assistant
                        // keeps its "polaris:" label since its lines are
                        // otherwise unmarked plain text.
                        let prefix = match m.role {
                            Role::User => String::new(),
                            _ => format!("{}: ", label(m.role)),
                        };
                        let mut in_code_block = false;
                        for (i, raw_line) in sanitized.split('\n').enumerate() {
                            if raw_line.trim_start().starts_with("```") {
                                in_code_block = !in_code_block;
                                lines.push(HistoryLine {
                                    line: Line::from(Span::styled(
                                        String::new(),
                                        Style::default().bg(Color::DarkGray),
                                    )),
                                    shaded: false,
                                });
                                continue;
                            }
                            let text = if i == 0 {
                                format!("{prefix}{raw_line}")
                            } else {
                                raw_line.to_string()
                            };
                            if in_code_block {
                                lines.push(HistoryLine {
                                    line: Line::from(Span::styled(
                                        text,
                                        Style::default().fg(color).bg(Color::DarkGray),
                                    )),
                                    shaded: false,
                                });
                            } else {
                                lines.push(HistoryLine {
                                    line: Line::from(format_inline(&text, color)),
                                    shaded,
                                });
                            }
                        }
                    }
                    lines
                }
            }
        })
        .collect()
}

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

/// Renders each `HistoryLine` into its own single-row slice of `buf`. One
/// `Paragraph` per row, not one `Paragraph` for the whole block — a
/// `Paragraph`'s background fill (`buf.set_style` over its full render
/// area, then text drawn on top) only covers the *entire area it's given*,
/// so giving each shaded row its own one-row area is what makes the gray
/// background span the full terminal width rather than stopping at the
/// text's own last character.
pub fn render_history_into(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    lines: &[HistoryLine],
) {
    use ratatui::widgets::Widget;
    for (i, hl) in lines.iter().enumerate() {
        let y_offset = i as u16;
        if y_offset >= area.height {
            break;
        }
        let row = ratatui::layout::Rect {
            x: area.x,
            y: area.y + y_offset,
            width: area.width,
            height: 1,
        };
        let style = if hl.shaded {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };
        Paragraph::new(hl.line.clone())
            .style(style)
            .render(row, buf);
        // Belt-and-braces: force the background on every cell of the row
        // directly, rather than relying solely on `Paragraph`'s own style
        // fill (`buf.set_style` over the render area) to survive intact
        // once this buffer is later flushed to a real terminal via
        // `Terminal::insert_before` — a real-terminal check (not caught by
        // any `TestBackend`-based test, since `TestBackend` records styled
        // cells directly with no ANSI round-trip to lose) found the fill
        // alone doesn't reliably reach the terminal for a shaded row once
        // scrolled into real scrollback, while explicitly setting each
        // cell's `bg` here does.
        if hl.shaded {
            for x in row.x..row.x + row.width {
                buf[(x, row.y)].set_bg(Color::DarkGray);
            }
        }
    }
}

/// What the status-bar header shows: which provider/model is in use, and
/// the token usage accumulated so far this session. This is a read-only
/// snapshot handed in by the caller each frame — `render.rs` never tracks
/// state itself.
pub struct HeaderInfo<'a> {
    pub provider_name: &'a str,
    pub model_name: &'a str,
    /// Shown next to `model_name` in the footer only (`gpt-5.6-sol high
    /// · ~/...`, per the same screenshot `cwd_short` cites) — the header
    /// box's `model:` row doesn't carry it, since it already has its own
    /// `/model to change` hint and adding effort there would duplicate
    /// what `/model`'s own picker screens already show.
    pub effort_name: &'a str,
    pub usage: polaris_provider::Usage,
    pub cwd: &'a str,
    /// `cwd`, but with a leading `$HOME` collapsed to `~` — shown only in
    /// the footer, matching codex's own footer (verified against a
    /// screenshot: `gpt-5.6-sol high · ~/File/projects/...`). The header
    /// box's `directory:` row keeps the full path in `cwd` instead, since
    /// there's no source/screenshot evidence codex abbreviates that one
    /// too and the full path is more useful there. `render.rs` can't
    /// compute this itself without reading `$HOME`, which would break
    /// its "no I/O, pure rendering" contract — the caller resolves it.
    pub cwd_short: &'a str,
}

/// Builds the bordered header box's lines — a dim border, a bold title
/// line, then dim-labeled `model:`/`directory:`/`tokens:` rows. Shared by
/// the one-shot startup print (`insert_before`, see `lib.rs`) and by tests
/// that want to check its content directly; there's exactly one caller
/// site for the actual print, since — unlike the old full-redraw model —
/// the header is now printed once and never redrawn.
pub fn header_lines(header: &HeaderInfo) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    vec![
        // A north-star glyph, not codex's own `>_` terminal-prompt mark —
        // "polaris" names the North Star, so the header's one brand mark
        // draws on that instead of reusing codex's. Gold rather than the
        // cyan used everywhere else for interactive hints (`/model to
        // change`, picker highlights), so this reads as a fixed identity
        // mark, not "something you can act on."
        Line::from(vec![
            Span::styled(
                "\u{2726} ",
                Style::default()
                    .fg(Color::Rgb(250, 204, 21))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("polaris", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("model:     ", dim),
            Span::raw(sanitize(header.model_name)),
            Span::raw(format!(" ({})   ", sanitize(header.provider_name))),
            Span::styled("/model", Style::default().fg(Color::Cyan)),
            Span::styled(" to change", dim),
        ]),
        Line::from(vec![
            Span::styled("directory: ", dim),
            Span::raw(sanitize(header.cwd)),
        ]),
        Line::from(vec![
            Span::styled("tokens:    ", dim),
            Span::styled(
                format!(
                    "in {} / out {} / total {}",
                    header.usage.input_tokens,
                    header.usage.output_tokens,
                    header.usage.total_tokens
                ),
                dim,
            ),
        ]),
    ]
}

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

/// The header box's fixed print height: 5 content lines + top/bottom
/// border rows (`Borders::ALL`).
pub const HEADER_HEIGHT: u16 = 7;

/// Renders `header_lines(header)` into `buf`, bordered — used for the
/// one-shot startup print via `insert_before`. `area` must be
/// `HEADER_HEIGHT` rows tall.
pub fn render_header_into(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    header: &HeaderInfo,
) {
    use ratatui::widgets::Widget;
    let dim = Style::default().add_modifier(Modifier::DIM);
    Paragraph::new(header_lines(header))
        .block(Block::default().borders(Borders::ALL).border_style(dim))
        .render(area, buf);
}

/// The most suggestion rows shown at once — the popup can't grow the
/// inline viewport's fixed height (ratatui's `Viewport::Inline` height is
/// fixed at `Terminal` construction, see `lib.rs`'s `INLINE_VIEWPORT_HEIGHT`),
/// so a query matching more than this many slash commands shows the first
/// `MAX_DISPLAYED_SUGGESTIONS` plus a "+N more" row instead of growing
/// without bound the way the old full-redraw layout allowed.
pub const MAX_DISPLAYED_SUGGESTIONS: usize = 8;

/// Everything redrawn every frame: the suggestions popup (while typing a
/// `/command`), the status row (idle / thinking-with-shimmer / error /
/// notice), the input line, and the footer (`model effort · cwd`). This is
/// what lives inside the fixed-height inline viewport — the header and the
/// conversation history are printed once, outside it, via `insert_before`
/// (see `lib.rs`), never redrawn here. Replaces the old `render_chat`,
/// which drew the header and full history inline with everything else on
/// every frame; splitting it out is what makes the header/history land in
/// the terminal's own real scrollback instead of being repainted away.
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
    let dim = Style::default().add_modifier(Modifier::DIM);
    // Zero height when there's nothing to show, so the layout collapses
    // back to the plain split the moment the input stops starting with
    // `/` — this row only exists while it has content. `+1` for the extra
    // "+N more" row once truncated.
    let shown = suggestions.len().min(MAX_DISPLAYED_SUGGESTIONS);
    let truncated = suggestions.len() > MAX_DISPLAYED_SUGGESTIONS;
    let suggestions_height = if suggestions.is_empty() {
        0
    } else {
        shown as u16 + 1 + u16::from(truncated)
    };
    let [status_area, suggestions_area, input_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(suggestions_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let status_line = match status {
        Status::Idle => Line::from(""),
        Status::Thinking { elapsed } => {
            let mut spans = shimmer_spans("Working", *elapsed);
            spans.push(Span::styled(
                format!(" ({}s \u{b7} esc to interrupt)", elapsed.as_secs()),
                Style::default().add_modifier(Modifier::DIM),
            ));
            Line::from(spans)
        }
        Status::Error(e) => Line::from(sanitize(&format!("error: {e}"))),
        Status::Notice(n) => Line::from(sanitize(n)),
    };
    frame.render_widget(Paragraph::new(status_line), status_area);

    if !suggestions.is_empty() {
        // Mirrors codex's own picker: the highlighted row's name is bold
        // cyan, its description full-brightness; every other row's
        // description is dimmed. `\u{2191}/\u{2193}` moves the highlight,
        // Enter accepts whichever row is currently marked — typing the
        // full name is still possible but no longer required. Capped at
        // `MAX_DISPLAYED_SUGGESTIONS` (see its doc) since the inline
        // viewport's height can't grow to fit an unbounded match list the
        // way the old fullscreen layout could.
        let mut lines: Vec<Line> = suggestions
            .iter()
            .take(shown)
            .enumerate()
            .map(|(i, c)| {
                let is_selected = i == selected_suggestion;
                let marker = if is_selected { "\u{203a} " } else { "  " };
                let name_style = if is_selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let desc_style = if is_selected {
                    Style::default()
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(sanitize(&format!("/{:<8}", c.name)), name_style),
                    Span::styled(sanitize(c.description), desc_style),
                ])
            })
            .collect();
        if truncated {
            lines.push(Line::from(Span::styled(
                format!("  \u{2026} {} more, keep typing", suggestions.len() - shown),
                Style::default().fg(Color::DarkGray),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::TOP)),
            suggestions_area,
        );
    }

    // No box around the input line — a bare `\u{203a} ` prompt, matching
    // codex's own composer, with a dim placeholder while empty instead of
    // an empty bordered box. The whole row is shaded gray (via the
    // `Paragraph`'s own `.style()`, which fills its entire render area —
    // not just the styled Spans' own cells) matching codex's own input-box
    // styling, and matching how a submitted line looks once it's printed
    // into history (`HistoryLine::shaded`, set for `Role::User`).
    let input_line = if input.is_empty() {
        Line::from(vec![
            Span::styled(
                "\u{203a} ",
                Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::styled(
                "Ask polaris to do anything",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "\u{203a} ",
                Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::raw(sanitize(input)),
        ])
    };
    frame.render_widget(
        Paragraph::new(input_line).style(Style::default().bg(Color::DarkGray)),
        input_area,
    );

    // Places the real terminal cursor at `cursor`'s position within the
    // typed text, right after the "\u{203a} " prompt. Terminal emulators
    // anchor the OS/IME preedit (未確定文字) popup to this reported
    // position, not to wherever text visually appears — if the app never
    // reports where the caret actually is, the cursor stays wherever the
    // last raw write left it, which produced an IME composition window
    // floating at the screen's bottom edge instead of sitting after the
    // typed text. `"› "`'s display width is 2 (both cells are
    // single-width), matching the literal used in `input_line` above;
    // `UnicodeWidthStr::width` (not `.chars().count()`) accounts for wide
    // (CJK) characters already typed before `cursor` so the reported
    // column lines up with what's actually drawn; the `.min(...)` clamp
    // keeps it from running past the row's right edge.
    let prefix_width: u16 = 2;
    let typed_width = sanitize(&input[..cursor]).width() as u16;
    let cursor_x = input_area
        .x
        .saturating_add(prefix_width)
        .saturating_add(typed_width)
        .min(input_area.right().saturating_sub(1));
    frame.set_cursor_position(Position::new(cursor_x, input_area.y));

    // Two-tone footer matching codex's own status line: the model name in
    // a warm tan, the working directory in a soft green, separated by a
    // dim middle dot.
    let footer_line = Line::from(vec![
        Span::styled(
            format!(
                "{} {}",
                sanitize(header.model_name),
                sanitize(header.effort_name)
            ),
            Style::default().fg(Color::Rgb(246, 226, 183)),
        ),
        Span::styled(" \u{b7} ", dim),
        Span::styled(
            sanitize(header.cwd_short),
            Style::default().fg(Color::Rgb(171, 223, 167)),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer_line), footer_area);
}

/// Renders the `/resume` picker: every saved conversation, grouped by
/// originating directory (current directory's group first — see
/// `sessions::grouped`), with the highlighted row marked the same way
/// the slash-command popup marks its selection. `selected` indexes into
/// the flattened session list (group headers aren't selectable and
/// don't count), matching how the caller's own navigation counts rows.
/// `now_millis` is supplied by the caller (not read here) for the same
/// reason `shimmer_spans` takes `elapsed` instead of a clock — it keeps
/// this pure/testable. Row timestamps and the marker/footer conventions
/// (relative "Ns/Nm/Nh/Nd ago" labels, a `❯` selection marker, a
/// bottom hint bar) were verified against codex's own resume picker
/// (`resume_picker.rs` and its test snapshots) rather than guessed —
/// codex's own picker is a richer sortable/searchable table this only
/// borrows the row/footer conventions from, not the full feature set.
pub fn render_resume_picker(
    frame: &mut Frame,
    groups: &[crate::sessions::DirGroup],
    selected: usize,
    now_millis: u128,
) {
    let area = frame.area();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "resume a conversation",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if groups.is_empty() {
        lines.push(Line::from(Span::styled("no saved conversations yet", dim)));
    }

    let mut idx = 0usize;
    for group in groups {
        lines.push(Line::from(Span::styled(
            sanitize(&group.cwd),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for s in &group.sessions {
            let is_selected = idx == selected;
            let marker = if is_selected { "\u{276f} " } else { "  " };
            let when = crate::time::format_relative(s.started_at_millis, now_millis);
            let text = format!(
                "{marker}{when}  {} message(s)  {}",
                s.message_count,
                sanitize(&s.preview)
            );
            let style = if is_selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            lines.push(Line::from(Span::styled(text, style)));
            idx += 1;
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "enter to resume \u{b7} esc to cancel",
        dim,
    )));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// Fixed model choices for the `/model` picker. Not a live catalog query
/// (polaris has no models-list endpoint integration) — this is the same
/// name set shown in a real codex `/model` picker screenshot, minus
/// `gpt-daybreak-blue-latest` (a specialized variant not representative
/// of general use) and minus reasoning effort (a dimension polaris's
/// `CompletionRequest` doesn't carry at all). `pub` so `lib.rs`'s
/// `run_model_picker` can navigate it without duplicating the list.
pub const MODEL_CATALOG: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
];

/// The `/model` picker: numbered options, arrow-key navigable, the
/// currently-active one marked `(current)` — the same layout
/// `render_permissions_picker` uses, both modeled on a screenshot of
/// codex's real `/model` picker. Unlike codex's, this carries no
/// per-model description text: codex's descriptions ("Latest frontier
/// agentic coding model", ...) are claims about that specific catalog
/// that polaris has no way to verify for whatever endpoint
/// `POLARIS_BASE_URL` actually points at, so making the same claims here
/// would be asserting something unverified.
pub fn render_model_picker(frame: &mut Frame, current: &str, selected: usize) {
    let area = frame.area();
    let mut lines: Vec<Line> = vec![
        Line::styled(
            "choose what model to use",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];
    for (i, name) in MODEL_CATALOG.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let suffix = if *name == current { " (current)" } else { "" };
        let style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{}. {}{suffix}", i + 1, sanitize(name)), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "enter to confirm \u{b7} esc to cancel",
        Style::default().add_modifier(Modifier::DIM),
    ));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// Reasoning-effort choices for the `/model` wizard's second step, in
/// the order codex's real "Select Reasoning Level" screen lists them —
/// verified against two screenshots of it. codex splits this into two
/// screens (`Low`..`Extra high` on the main list, then a `More
/// reasoning...` row that opens a second "Advanced Reasoning" screen
/// for `Max`/`Ultra`, each carrying a "consumes usage limits faster"
/// warning); explicit instruction here was to list everything in one
/// flat picker instead, so all six are just rows 1-6 of the same list.
pub const EFFORT_CATALOG: &[(&str, &str)] = &[
    ("low", "fast responses with lighter reasoning"),
    (
        "medium",
        "balances speed and reasoning depth for everyday tasks",
    ),
    ("high", "greater reasoning depth for complex problems"),
    (
        "extra high",
        "extra reasoning depth for complex problems \u{b7} consumes usage limits faster",
    ),
    (
        "max",
        "for difficult problems when quality matters more than speed \u{b7} consumes usage limits faster",
    ),
    (
        "ultra",
        "for the most demanding work \u{b7} consumes usage limits faster",
    ),
];

/// `EFFORT_CATALOG`'s first entry is always the default, the same way
/// codex marks `Low` `(default)` regardless of which row is currently
/// active.
pub const DEFAULT_EFFORT: &str = EFFORT_CATALOG[0].0;

/// Maps a picker-level effort name to the literal token actually sent
/// over the wire. Real reasoning-effort APIs only accept a handful of
/// space-free values — `polaris_auth::effort_for_plan_type` is
/// first-party evidence for exactly which ones (`"low"`/`"xhigh"`, with
/// `"medium"`/`"high"` filling the gap between). Sending `EFFORT_CATALOG`'s
/// display name directly, `"extra high"` included, is what produced a
/// real `400 Invalid value: 'extra high'` from the API — this exists so
/// that bug can't recur for any entry. `"max"`/`"ultra"` are a
/// ChatGPT-app-only upsell tier the API has no literal enum value for at
/// all (there's no evidence anywhere in this codebase of a wire token
/// beyond `xhigh`), so both fall back to the highest value that's
/// actually valid rather than guessing a new string and hitting the same
/// 400 `"extra high"` did.
pub fn effort_wire_value(name: &str) -> &str {
    match name {
        "extra high" | "max" | "ultra" => "xhigh",
        other => other,
    }
}

/// The `/model` wizard's second step — picking the reasoning effort for
/// whichever model was just picked in `render_model_picker`. Same
/// numbered/highlighted layout, plus a `(default)`/`(current)` suffix
/// (both, space-separated, when a row is both — `low` starts out as
/// both at once).
pub fn render_effort_picker(frame: &mut Frame, model: &str, current: &str, selected: usize) {
    let area = frame.area();
    let mut lines: Vec<Line> = vec![
        Line::styled(
            format!("choose the reasoning effort for {}", sanitize(model)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];
    for (i, (name, description)) in EFFORT_CATALOG.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let is_default = *name == DEFAULT_EFFORT;
        let is_current = *name == current;
        let suffix = match (is_default, is_current) {
            (true, true) => " (default, current)",
            (true, false) => " (default)",
            (false, true) => " (current)",
            (false, false) => "",
        };
        let name_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let desc_style = if is_selected {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{}. {}{suffix}", i + 1, sanitize(name)), name_style),
            Span::raw("  "),
            Span::styled(sanitize(description), desc_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "enter to confirm \u{b7} esc to go back",
        Style::default().add_modifier(Modifier::DIM),
    ));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// The `/skills` picker: every skill discovered in this project, browsable
/// (arrow keys move the highlight) but not actionable — polaris has no
/// enable/disable concept for a skill the way codex's own `/skills` →
/// "Enable/Disable Skills" screen does, so Enter and Esc both just close
/// it. Still worth its own full-screen view rather than the one-line,
/// six-name-then-"+N more" `Status::Notice` it replaces: with more than a
/// handful of skills, that line either got truncated or just didn't fit.
pub fn render_skills_picker(frame: &mut Frame, skills: &[polaris_skills::Skill], selected: usize) {
    let area = frame.area();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines: Vec<Line> = vec![
        Line::styled(
            "skills discovered in this project",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];

    if skills.is_empty() {
        lines.push(Line::styled("no skills discovered", dim));
    }

    for (i, s) in skills.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let name_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(sanitize(&s.name), name_style),
        ]));
        lines.push(Line::styled(
            format!("     {}", sanitize(&s.description)),
            dim,
        ));
    }

    lines.push(Line::from(""));
    lines.push(Line::styled("enter or esc to close", dim));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// The three `ApprovalPolicy` choices, in the order the `/permissions`
/// picker lists them, alongside the one-line description each already
/// carries in `polaris_core::approval`'s own doc comments.
const PERMISSIONS_OPTIONS: [(polaris_core::approval::ApprovalPolicy, &str, &str); 3] = [
    (
        polaris_core::approval::ApprovalPolicy::Never,
        "never",
        "never ask — anything out of scope is refused outright",
    ),
    (
        polaris_core::approval::ApprovalPolicy::OnRequest,
        "on request",
        "ask only when a mutating operation is out of scope",
    ),
    (
        polaris_core::approval::ApprovalPolicy::Always,
        "always",
        "ask before every mutating operation",
    ),
];

/// The `/permissions` picker: three numbered options, arrow-key
/// navigable, the currently-active one marked `(current)` — mirroring
/// the numbered/highlighted layout codex's own `/model` picker uses
/// (verified against a screenshot of it), applied here to something
/// polaris already has state for (`ApprovalPolicy`) rather than the
/// model list polaris can't actually switch mid-session yet.
pub fn render_permissions_picker(
    frame: &mut Frame,
    current: polaris_core::approval::ApprovalPolicy,
    selected: usize,
) {
    let area = frame.area();
    let mut lines: Vec<Line> = vec![
        Line::styled(
            "choose what polaris is allowed to do",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];
    for (i, (policy, name, description)) in PERMISSIONS_OPTIONS.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let suffix = if *policy == current { " (current)" } else { "" };
        let title_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{}. {name}{suffix}", i + 1), title_style),
        ]));
        lines.push(Line::styled(
            format!("     {description}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "enter to confirm \u{b7} esc to cancel",
        Style::default().add_modifier(Modifier::DIM),
    ));

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

pub fn render_approval_modal(frame: &mut Frame, reason: &str) {
    let area = frame.area();
    let text = format!(
        "Approval required: {}\n\n[y] allow   [n] deny",
        sanitize(reason)
    );
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

    /// Renders `messages` the same way `print_new_history` does in
    /// production (`history_lines_for` then `render_history_into`), into a
    /// buffer tall enough to hold every line with no clipping — the
    /// one-shot print model has no "does it fit" concept the way the old
    /// bounded/scrolled history pane did, so tests don't need to reason
    /// about a viewport height either.
    fn render_history_to_string(messages: &[polaris_provider::Message], width: u16) -> String {
        let lines = history_lines_for(messages);
        let height = (lines.len() as u16).max(1);
        let mut buf =
            ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, width, height));
        let area = buf.area;
        render_history_into(&mut buf, area, &lines);
        buf.content.iter().map(|c| c.symbol()).collect::<String>()
    }

    /// Renders the always-redrawn footer (suggestions/status/input/footer)
    /// via a real `Frame`, the same way `lib.rs`'s per-frame `draw()` call
    /// does.
    fn render_footer_to_string(
        input: &str,
        status: &Status,
        header: &HeaderInfo,
        suggestions: &[&crate::slash::SlashCommand],
        selected_suggestion: usize,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render_footer(
                    f,
                    input,
                    input.len(),
                    status,
                    header,
                    suggestions,
                    selected_suggestion,
                )
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }

    /// Renders the footer with an explicit cursor byte-offset and returns
    /// where the real terminal cursor landed.
    fn render_footer_cursor_position(
        input: &str,
        cursor: usize,
        width: u16,
        height: u16,
    ) -> ratatui::layout::Position {
        let header = test_header();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_footer(f, input, cursor, &Status::Idle, &header, &[], 0))
            .expect("draw");
        terminal.get_cursor_position().expect("cursor position")
    }

    /// Regression test for the IME-preedit-shows-at-the-bottom bug: the
    /// terminal cursor must land exactly after the typed text in the
    /// input row, not stay wherever a prior raw write left it (which
    /// `Frame::set_cursor_position` never being called defaulted to
    /// "hidden, unpositioned" — see `terminal.rs`'s `try_draw`).
    #[test]
    fn the_cursor_lands_right_after_the_typed_input_text() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render_footer(f, "hello", 5, &Status::Idle, &test_header(), &[], 0);
            })
            .expect("draw");
        // "› " (width 2) + "hello" (width 5) = column 7, on the input row
        // (status(1) + suggestions(0) = row 1).
        terminal.backend_mut().assert_cursor_position((7, 1));
    }

    /// Wide (CJK) characters must count as 2 columns each, or the cursor
    /// — and thus the IME popup a terminal anchors to it — would land
    /// short of the actual caret whenever any wide character was already
    /// typed, exactly the scenario a real Japanese IME composition hits.
    #[test]
    fn the_cursor_accounts_for_wide_characters_already_typed() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let input = "helloこんにちは";
        terminal
            .draw(|f| {
                render_footer(f, input, input.len(), &Status::Idle, &test_header(), &[], 0);
            })
            .expect("draw");
        // "› "(2) + "hello"(5) + "こんにちは"(5 chars * width 2 = 10) = 17.
        terminal.backend_mut().assert_cursor_position((17, 1));
    }

    fn test_header() -> HeaderInfo<'static> {
        HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        }
    }

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

    #[test]
    fn a_user_message_appears_unlabeled_and_shaded() {
        // No "you:" label — the full-row gray shading (`shaded: true`) is
        // the only marker, matching codex's own unlabeled user line.
        let mut session = Session::default();
        session.push_user("Cargo.toml は何行か");

        let content = render_history_to_string(&session.messages, 60);
        assert!(!content.contains("you:"));
        assert!(content.contains("Cargo.toml"));
        let lines = history_lines_for(&session.messages);
        assert!(lines[0].shaded);
    }

    #[test]
    fn the_status_line_shows_thinking_while_a_turn_is_in_flight() {
        let header = test_header();
        let status = Status::Thinking {
            elapsed: Duration::from_secs(3),
        };
        let content = render_footer_to_string("", &status, &header, &[], 0, 60, 6);
        assert!(content.contains("Working"));
        assert!(content.contains("3s"));
        assert!(content.contains("esc to interrupt"));
    }

    #[test]
    fn the_cursor_lands_right_after_the_prompt_prefix_when_the_buffer_is_empty() {
        let pos = render_footer_cursor_position("", 0, 60, 6);
        // status(1) + suggestions(0, empty when no popup) puts the input
        // row at index 1.
        assert_eq!(pos.y, 1);
        assert_eq!(pos.x, Line::from("\u{203a} ").width() as u16);
    }

    #[test]
    fn the_cursor_tracks_a_mid_buffer_offset_not_always_the_end() {
        let full = render_footer_cursor_position("hello", 5, 60, 6);
        let mid = render_footer_cursor_position("hello", 2, 60, 6);
        assert_eq!(full.x - mid.x, 3);
    }

    #[test]
    fn the_cursor_accounts_for_wide_characters_before_it() {
        // Hiragana "あ" occupies 2 terminal columns, unlike the following
        // "b" — the cursor after both must be 2 (prefix) + 2 (wide) + 1
        // columns ahead of the prefix-only position, not 2 (prefix) + 2
        // (one char each).
        let input = "\u{3042}b";
        let after_wide_char = "\u{3042}".len();
        let pos = render_footer_cursor_position(input, after_wide_char, 60, 6);
        let prefix = render_footer_cursor_position(input, 0, 60, 6);
        assert_eq!(pos.x - prefix.x, 2);
    }

    #[test]
    fn shimmer_spans_reproduce_the_text_verbatim() {
        // The animation only ever changes color/weight — concatenating
        // the spans back together must always yield the original text,
        // never dropped or reordered characters.
        let spans = shimmer_spans("Working", Duration::from_millis(750));
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Working");
    }

    #[test]
    fn shimmer_spans_on_empty_text_is_empty() {
        assert!(shimmer_spans("", Duration::ZERO).is_empty());
    }

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

    #[test]
    fn history_longer_than_the_old_screen_height_is_never_clipped() {
        // The one-shot print model (`insert_before`, see `lib.rs`) has no
        // bounded viewport to scroll within — every message is printed
        // once, unbounded, into the terminal's own real scrollback. This
        // replaces the old test of the same name's intent
        // ("does the newest message stay visible"), which assumed a fixed
        // history pane height that no longer exists: the *point* of this
        // whole feature is that history isn't clipped by screen height at
        // all anymore, so the correct assertion is now the opposite of
        // the old one — the earliest message is still present, not
        // scrolled away.
        let mut session = Session::default();
        for i in 0..30 {
            session.push_user(&format!("message number {i}"));
        }

        let content = render_history_to_string(&session.messages, 60);
        assert!(content.contains("message number 29"));
        assert!(content.contains("message number 0 "));
    }

    #[test]
    fn the_resume_picker_shows_no_saved_conversations_when_there_are_none() {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_resume_picker(f, &[], 0, 0))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("no saved conversations"));
    }

    #[test]
    fn the_resume_picker_lists_directories_and_previews() {
        let now = 1_705_311_000_000u128;
        let groups = vec![crate::sessions::DirGroup {
            cwd: "/tmp/example".to_string(),
            sessions: vec![crate::sessions::SessionSummary {
                id: "s1".to_string(),
                path: std::path::PathBuf::new(),
                cwd: "/tmp/example".to_string(),
                started_at_millis: now - 42_000,
                message_count: 3,
                preview: "what does this repo do?".to_string(),
            }],
        }];

        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_resume_picker(f, &groups, 0, now))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("/tmp/example"));
        // Relative, not absolute — matches codex's own resume-picker
        // convention ("42s ago"), verified against its real source.
        assert!(content.contains("42s ago"));
        assert!(content.contains("3 message"));
        assert!(content.contains("what does this repo do?"));
        assert!(content.contains("enter to resume"));
        assert!(content.contains("esc to cancel"));
    }

    #[test]
    fn the_model_picker_marks_the_current_model_and_highlights_the_selection() {
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_model_picker(f, "gpt-5.4", 4))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(rows.iter().any(|r| r.contains("gpt-5.4 (current)")));
        assert!(!rows.iter().any(|r| r.contains("gpt-5.5 (current)")));
        assert!(rows.iter().any(|r| r.contains("enter to confirm")));
    }

    #[test]
    fn the_effort_picker_lists_every_level_in_one_flat_list() {
        // The explicit instruction was "don't split max/ultra onto a
        // separate page" — this just confirms all six still show up on
        // one screen, since a regression here would silently drop that.
        let backend = TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_effort_picker(f, "gpt-5.6-sol", "low", 0))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        for (name, _) in EFFORT_CATALOG {
            assert!(content.contains(name), "{name} should be listed");
        }
        assert!(content.contains("gpt-5.6-sol"));
    }

    #[test]
    fn the_effort_picker_marks_default_and_current_distinctly() {
        let backend = TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_effort_picker(f, "gpt-5.6-sol", "high", 0))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(rows.iter().any(|r| r.contains("low (default)")));
        assert!(rows.iter().any(|r| r.contains("high (current)")));
        // Neither row is both at once here, so the combined suffix must
        // not appear.
        assert!(!rows.iter().any(|r| r.contains("(default, current)")));
    }

    #[test]
    fn the_effort_picker_combines_default_and_current_on_the_same_row() {
        let backend = TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_effort_picker(f, "gpt-5.6-sol", "low", 0))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("low (default, current)"));
    }

    #[test]
    fn effort_wire_value_maps_every_catalog_entry_to_something_space_free() {
        // The regression this guards: `EFFORT_CATALOG`'s display name
        // ("extra high") used to be sent to the API directly and got a
        // real `400 Invalid value: 'extra high'` back. No entry's wire
        // value may contain a space, ever again.
        for (name, _) in EFFORT_CATALOG {
            let wire = effort_wire_value(name);
            assert!(
                !wire.contains(' '),
                "{name} maps to {wire:?}, which contains a space"
            );
        }
    }

    #[test]
    fn effort_wire_value_leaves_already_valid_names_unchanged() {
        assert_eq!(effort_wire_value("low"), "low");
        assert_eq!(effort_wire_value("medium"), "medium");
        assert_eq!(effort_wire_value("high"), "high");
    }

    #[test]
    fn effort_wire_value_maps_extra_high_max_and_ultra_to_xhigh() {
        assert_eq!(effort_wire_value("extra high"), "xhigh");
        assert_eq!(effort_wire_value("max"), "xhigh");
        assert_eq!(effort_wire_value("ultra"), "xhigh");
    }

    #[test]
    fn the_skills_picker_lists_every_skill_with_its_description() {
        let skills = vec![
            polaris_skills::Skill {
                name: "reviewer".to_string(),
                description: "reviews code for bugs".to_string(),
                body: String::new(),
                path: std::path::PathBuf::new(),
            },
            polaris_skills::Skill {
                name: "planner".to_string(),
                description: "breaks work into steps".to_string(),
                body: String::new(),
                path: std::path::PathBuf::new(),
            },
        ];
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_skills_picker(f, &skills, 0))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("reviewer"));
        assert!(content.contains("reviews code for bugs"));
        assert!(content.contains("planner"));
        assert!(content.contains("breaks work into steps"));
    }

    #[test]
    fn the_skills_picker_shows_a_message_when_there_are_none() {
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_skills_picker(f, &[], 0))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("no skills discovered"));
    }

    #[test]
    fn the_permissions_picker_marks_the_currently_active_policy() {
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render_permissions_picker(f, polaris_core::approval::ApprovalPolicy::Always, 0)
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
        assert!(rows.iter().any(|r| r.contains("always (current)")));
        assert!(!rows.iter().any(|r| r.contains("never (current)")));
    }

    #[test]
    fn the_permissions_picker_highlights_whichever_option_is_selected() {
        let backend = TestBackend::new(70, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render_permissions_picker(f, polaris_core::approval::ApprovalPolicy::Never, 2)
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let find_title_color = |needle: &str| {
            for y in 0..buffer.area.height {
                let cells: Vec<&str> = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                let row = cells.concat();
                if let Some(byte_col) = row.find(needle) {
                    // `byte_col` is a byte offset into the concatenated
                    // row string; since every cell here is ASCII, that's
                    // also the cell/column index.
                    return buffer[(byte_col as u16, y)].fg;
                }
            }
            panic!("row containing {needle:?} not found");
        };
        // The highlighted row (index 2, "always") is cyan; an
        // unselected row isn't.
        assert_eq!(find_title_color("always"), Color::Cyan);
        assert_ne!(find_title_color("never"), Color::Cyan);
    }

    #[test]
    fn the_approval_modal_shows_the_reason() {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_approval_modal(f, "writing to src/main.rs"))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("writing to src/main.rs"));
    }

    #[test]
    fn a_message_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
        let mut session = Session::default();
        session.push_assistant("\x1b[31mfake red\x1b[0m");

        let content = render_history_to_string(&session.messages, 60);
        assert!(!content.chars().any(|c| c == '\u{1b}'));
        // The rest of the text should still be visible, just with the
        // control bytes neutralized rather than the whole message dropped.
        assert!(content.contains("fake red"));
    }

    /// Concatenates every span of every line back into one string, the
    /// way the terminal would print them.
    fn live_print_text(event: &AgentEvent) -> String {
        format_event_for_live_print(event)
            .iter()
            .flat_map(|l| l.line.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn a_tool_detail_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
        // Tool activity no longer goes through `history_lines_for` (it's
        // printed live instead), so the escape-smuggling guard that used
        // to be checked on a `Role::Tool` message now belongs on the
        // live-print path.
        let text = live_print_text(&AgentEvent::ToolStarted {
            name: "bash".to_string(),
            detail: "\x1b[31mfake\x1b[0m".to_string(),
        });
        assert!(!text.chars().any(|c| c == '\u{1b}'));
        // The rest of the text should still be visible, just with the
        // control bytes neutralized rather than the whole detail being
        // dropped.
        assert!(text.contains("fake"));
    }

    #[test]
    fn a_tool_started_event_renders_as_a_bullet_line() {
        let lines = format_event_for_live_print(&AgentEvent::ToolStarted {
            name: "bash".to_string(),
            detail: "{\"command\":\"ls\"}".to_string(),
        });
        let text: String = lines[0]
            .line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("⏺"));
        assert!(text.contains("bash"));
        assert!(text.contains("ls"));
    }

    #[test]
    fn a_tool_finished_event_reports_success_or_failure() {
        let ok = live_print_text(&AgentEvent::ToolFinished {
            name: "bash".to_string(),
            detail: "hello".to_string(),
            ok: true,
            result: String::new(),
            diff: None,
        });
        assert!(ok.contains("done"));
        let failed = live_print_text(&AgentEvent::ToolFinished {
            name: "bash".to_string(),
            detail: "boom".to_string(),
            ok: false,
            result: String::new(),
            diff: None,
        });
        assert!(failed.contains("failed"));
    }

    #[test]
    fn a_tool_finished_event_previews_its_result_text() {
        let text = live_print_text(&AgentEvent::ToolFinished {
            name: "read".to_string(),
            detail: "{\"path\":\"a.txt\"}".to_string(),
            ok: true,
            result: "     1\thello from the file".to_string(),
            diff: None,
        });
        assert!(text.contains("done"));
        assert!(text.contains("hello from the file"));
        // タブは展開されてから切り詰められる
        assert!(!text.contains('\t'));
    }

    #[test]
    fn a_long_tool_result_preview_is_truncated_at_the_shared_budget() {
        let text = live_print_text(&AgentEvent::ToolFinished {
            name: "bash".to_string(),
            detail: "{}".to_string(),
            ok: true,
            result: "x".repeat(TOOL_RESULT_PREVIEW_CHARS * 3),
            diff: None,
        });
        assert!(text.contains(&"x".repeat(TOOL_RESULT_PREVIEW_CHARS)));
        assert!(!text.contains(&"x".repeat(TOOL_RESULT_PREVIEW_CHARS + 1)));
        assert!(text.contains("..."));
    }

    #[test]
    fn a_failed_tool_previews_its_error_message() {
        let text = live_print_text(&AgentEvent::ToolFinished {
            name: "write".to_string(),
            detail: "{}".to_string(),
            ok: false,
            result: "permission denied".to_string(),
            diff: None,
        });
        assert!(text.contains("failed"));
        assert!(text.contains("permission denied"));
    }

    #[test]
    fn an_empty_tool_result_adds_no_preview_row() {
        let lines = format_event_for_live_print(&AgentEvent::ToolFinished {
            name: "write".to_string(),
            detail: "{}".to_string(),
            ok: true,
            result: String::new(),
            diff: None,
        });
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn a_multiline_tool_result_preview_is_split_into_rows_and_capped() {
        let lines = format_event_for_live_print(&AgentEvent::ToolFinished {
            name: "bash".to_string(),
            detail: "{}".to_string(),
            ok: true,
            result: (0..50)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            diff: None,
        });
        for l in &lines {
            let text: String = l.line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(!text.contains('\n'), "a row still carries a newline");
        }
        // done行 + プレビュー(最大SPAWN_TASK_PREVIEW_LINES行 + 省略行)
        assert!(lines.len() <= 1 + SPAWN_TASK_PREVIEW_LINES + 1);
    }

    #[test]
    fn a_tool_finished_event_with_a_diff_renders_added_and_removed_lines_with_a_header() {
        let diff = polaris_core::Diff {
            is_new_file: false,
            hunks: vec![polaris_core::DiffHunk {
                lines: vec![
                    polaris_core::DiffLine::Context("unchanged".to_string()),
                    polaris_core::DiffLine::Removed("old line".to_string()),
                    polaris_core::DiffLine::Added("new line".to_string()),
                ],
            }],
            added: 1,
            removed: 1,
        };
        let event = AgentEvent::ToolFinished {
            name: "write".to_string(),
            detail: "{\"path\":\"a.txt\"}".to_string(),
            ok: true,
            result: String::new(),
            diff: Some(diff),
        };
        let joined = live_print_text(&event);
        assert!(joined.contains("Added 1 lines, removed 1 lines"));
        assert!(joined.contains("old line"));
        assert!(joined.contains("new line"));
    }

    #[test]
    fn a_diff_longer_than_the_cap_is_truncated_with_a_notice() {
        let many_added: Vec<polaris_core::DiffLine> = (0..100)
            .map(|i| polaris_core::DiffLine::Added(format!("line {i}")))
            .collect();
        let diff = polaris_core::Diff {
            is_new_file: true,
            hunks: vec![polaris_core::DiffHunk { lines: many_added }],
            added: 100,
            removed: 0,
        };
        let event = AgentEvent::ToolFinished {
            name: "write".to_string(),
            detail: "{\"path\":\"big.txt\"}".to_string(),
            ok: true,
            result: String::new(),
            diff: Some(diff),
        };
        let lines = format_event_for_live_print(&event);
        // Header row + done/failed row + up to MAX_DIFF_LINES_SHOWN diff
        // rows + one elision row — well under the 100 lines the diff
        // itself carries.
        assert!(lines.len() < 100);
        let joined = live_print_text(&event);
        assert!(joined.contains("省略"));
    }

    #[test]
    fn a_spawn_started_event_mentions_backgrounded_agent() {
        let joined = live_print_text(&AgentEvent::SpawnStarted {
            agent_type: "file-inspector".to_string(),
            task: "inspect agent.rs".to_string(),
        });
        assert!(joined.contains("Agent"));
        assert!(joined.contains("file-inspector"));
        assert!(joined.contains("Backgrounded agent"));
    }

    #[test]
    fn a_multi_line_spawn_task_is_split_across_rows_not_left_in_one_line() {
        // `render_history_into` reserves exactly one terminal row per
        // `HistoryLine`, so a raw `\n` surviving inside one line would be
        // written into the middle of a single-row slot. Every physical
        // line of the task must get its own `HistoryLine` instead.
        let lines = format_event_for_live_print(&AgentEvent::SpawnStarted {
            agent_type: "file-inspector".to_string(),
            task: "first line\nsecond line\nthird line".to_string(),
        });
        let rows: Vec<String> = lines
            .iter()
            .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        assert!(rows.iter().all(|r| !r.contains('\n')), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("first line")));
        assert!(rows.iter().any(|r| r.contains("second line")));
        assert!(rows.iter().any(|r| r.contains("third line")));
        // The three task lines land on three distinct rows, and the
        // "Backgrounded agent" follow-up is still its own row after them.
        assert_eq!(rows.len(), 4);
        assert!(rows[0].contains("⏺ Agent(file-inspector: first line"));
        assert!(rows[3].contains("Backgrounded agent"));
    }

    #[test]
    fn a_pathologically_multi_line_spawn_task_is_capped_at_a_few_rows() {
        let lines = format_event_for_live_print(&AgentEvent::SpawnStarted {
            agent_type: "file-inspector".to_string(),
            task: "x\n".repeat(80),
        });
        // At most the capped task rows, one `...` elision row, and the
        // "Backgrounded agent" row — never one row per task line.
        assert_eq!(lines.len(), SPAWN_TASK_PREVIEW_LINES + 2);
        let last_task_rows: String = lines[SPAWN_TASK_PREVIEW_LINES]
            .line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last_task_rows.contains("..."));
    }

    #[test]
    fn a_spawn_finished_event_names_the_agent_type_and_its_outcome() {
        let joined = live_print_text(&AgentEvent::SpawnFinished {
            agent_type: "file-inspector".to_string(),
            ok: true,
        });
        assert!(joined.contains("file-inspector"));
        assert!(joined.contains("done"));
    }

    #[test]
    fn ordinary_text_renders_unaffected_by_sanitization() {
        let mut session = Session::default();
        session.push_user("plain ascii and 日本語 text, nothing weird here.");

        let content = render_history_to_string(&session.messages, 60);
        assert!(content.contains("plain ascii and"));
        assert!(content.contains("nothing weird here."));
    }

    #[test]
    fn an_approval_reason_with_a_control_character_is_sanitized() {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_approval_modal(f, "writing to \x1b]0;pwned\x07src/main.rs"))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!content.chars().any(|c| c == '\u{1b}'));
        assert!(content.contains("src/main.rs"));
    }

    #[test]
    fn a_status_error_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
        let header = test_header();
        let status = Status::Error("\x1b[31mfake\x1b[0m".to_string());

        let content = render_footer_to_string("", &status, &header, &[], 0, 60, 6);
        assert!(!content.chars().any(|c| c == '\u{1b}'));
        assert!(content.contains("error: "));
        assert!(content.contains("fake"));
    }

    #[test]
    fn a_multi_line_reply_renders_as_multiple_lines_not_one_clipped_line() {
        let mut session = Session::default();
        session.push_assistant("first paragraph\nsecond paragraph\nthird paragraph");

        let lines = history_lines_for(&session.messages);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            60,
            lines.len() as u16,
        ));
        let area = buf.area;
        render_history_into(&mut buf, area, &lines);
        // Every physical line of the message must land on its own row of
        // the rendered buffer, not be squashed onto a single row.
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        assert!(rows.iter().any(|r| r.contains("polaris: first paragraph")));
        assert!(rows.iter().any(|r| r.contains("second paragraph")));
        assert!(rows.iter().any(|r| r.contains("third paragraph")));
        // The label prefix should not repeat on continuation lines.
        assert!(!rows.iter().any(|r| r.contains("polaris: second paragraph")));
    }

    #[test]
    fn the_header_shows_provider_model_and_usage() {
        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: 140,
            },
        };

        // Wide enough that the header box's one content line ("model: ...
        // tokens: in ... / out ... / total ...") isn't cut off before the
        // usage numbers this test asserts on.
        let mut buf =
            ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, HEADER_HEIGHT));
        let area = buf.area;
        render_header_into(&mut buf, area, &header);

        let content = buf.content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("openai"));
        assert!(content.contains("gpt-5.4"));
        assert!(content.contains("140"));
    }

    #[test]
    fn the_footer_shows_the_model_and_effort_together() {
        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "high",
            provider_name: "openai",
            model_name: "gpt-5.6-sol",
            usage: polaris_provider::Usage::default(),
        };
        let content = render_footer_to_string("", &Status::Idle, &header, &[], 0, 80, 3);
        // Matches the verified screenshot's footer shape: "{model}
        // {effort} · {cwd}".
        assert!(content.contains("gpt-5.6-sol high"));
        assert!(content.contains("~/example"));
    }

    #[test]
    fn tool_calls_and_tool_results_are_left_out_of_the_batch_printed_history() {
        // Tool activity is printed live from the `AgentEvent` stream
        // while the turn runs (`format_event_for_live_print`), so the
        // post-turn batch print must not repeat it — only the user's and
        // the assistant's own text belongs here.
        let mut session = Session::default();
        session.push_user("what's in a.txt?");
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
        );
        session.push_tool_result("c1", "hello from the tool");
        session.push_assistant("a.txt contains a greeting");

        let content = render_history_to_string(&session.messages, 60);
        assert!(
            !content.contains("read"),
            "the tool name must not be reprinted: {content}"
        );
        assert!(
            !content.contains("hello from the tool"),
            "the tool result must not be reprinted: {content}"
        );
        assert!(content.contains("what's in a.txt?"));
        assert!(content.contains("a.txt contains a greeting"));
    }

    #[test]
    fn an_assistant_message_carrying_only_tool_calls_produces_no_history_lines() {
        let mut session = Session::default();
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
        );
        session.push_tool_result("c1", "1\tfirst line\n2\tsecond line\n");

        assert!(history_lines_for(&session.messages).is_empty());
    }

    #[test]
    fn a_long_tool_detail_is_truncated_in_the_live_print() {
        let long_body: String = "x".repeat(500);
        let text = live_print_text(&AgentEvent::ToolStarted {
            name: "read".to_string(),
            detail: long_body.clone(),
        });
        // The full 500-character body must not appear verbatim; only a
        // prefix of it should.
        assert!(!text.contains(&long_body));
        assert!(text.contains(&"x".repeat(TOOL_RESULT_PREVIEW_CHARS)));
    }

    #[test]
    fn bold_text_is_rendered_with_the_bold_modifier() {
        let mut session = Session::default();
        session.push_assistant("this is **bold** text");

        let lines = history_lines_for(&session.messages);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            60,
            lines.len() as u16,
        ));
        let area = buf.area;
        render_history_into(&mut buf, area, &lines);
        let bold_cell = (0..buf.area.width)
            .flat_map(|x| (0..buf.area.height).map(move |y| (x, y)))
            .find(|&(x, y)| {
                buf[(x, y)].symbol() == "b" && {
                    let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                    row.contains("bold")
                }
            });
        let (x, y) = bold_cell.expect("the word 'bold' should appear somewhere");
        assert!(
            buf[(x, y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD),
            "the 'b' in 'bold' should carry the BOLD modifier"
        );
    }

    #[test]
    fn inline_code_and_surrounding_text_both_render_without_the_backticks() {
        let mut session = Session::default();
        session.push_assistant("run `cargo test` now");

        let content = render_history_to_string(&session.messages, 60);
        assert!(content.contains("cargo test"));
        assert!(!content.contains('`'));
    }

    #[test]
    fn user_and_assistant_lines_are_distinguished_by_shading_not_color() {
        // Both roles render in the terminal's own default foreground — no
        // tinted fg competes with the user line's full-row gray background
        // as a second, redundant "this is different" cue. The shading
        // itself (`HistoryLine::shaded`, checked directly on the built
        // lines rather than the rendered buffer) is what distinguishes
        // them.
        let mut session = Session::default();
        session.push_user("hello");
        session.push_assistant("hi there");

        let lines = history_lines_for(&session.messages);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            60,
            lines.len() as u16,
        ));
        let area = buf.area;
        render_history_into(&mut buf, area, &lines);
        let find_row_color = |needle: &str| {
            for y in 0..buf.area.height {
                let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                if row.contains(needle) {
                    return buf[(0, y)].fg;
                }
            }
            panic!("row containing {needle:?} not found");
        };
        assert_eq!(find_row_color("hello"), find_row_color("hi there"));
        assert!(lines.iter().any(|l| l.shaded));
        assert!(lines.iter().any(|l| !l.shaded));
    }

    #[test]
    fn sanitize_replaces_c0_controls_and_del_but_keeps_newline_and_tab() {
        let input = "a\x00b\x1bc\x7fd\ne\tf";
        let sanitized = sanitize(input);
        assert!(!sanitized.contains('\u{0}'));
        assert!(!sanitized.contains('\u{1b}'));
        assert!(!sanitized.contains('\u{7f}'));
        assert!(sanitized.contains('\n'));
        assert!(sanitized.contains('\t'));
        assert_eq!(sanitized, "a\u{fffd}b\u{fffd}c\u{fffd}d\ne\tf");
    }
}
