//! Pure rendering: turns a `Session` + input state into terminal cells.
//! Kept free of any actual terminal I/O so it's testable with
//! `ratatui::backend::TestBackend`.

use std::time::Duration;

use polaris_core::session::Session;
use polaris_provider::Role;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

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

fn format_tool_calls(calls: &[polaris_provider::ToolCall]) -> Vec<String> {
    calls
        .iter()
        .map(|c| {
            let args = serde_json::to_string(&c.arguments).unwrap_or_default();
            let args_preview: String = args.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            format!("⚙ {}({})", c.name, args_preview)
        })
        .collect()
}

fn format_tool_result(content: &str) -> String {
    // Expand tabs before truncating so the 200-char budget is spent on the
    // form actually rendered. A raw tab is effectively invisible in a
    // terminal grid (each cell is one character wide), which matters for
    // tools like `read` whose output separates line numbers from content
    // with `\t`.
    let expanded = content.replace('\t', "    ");
    let preview: String = expanded.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
    if expanded.chars().count() > TOOL_RESULT_PREVIEW_CHARS {
        format!("→ {preview}...")
    } else {
        format!("→ {preview}")
    }
}

fn role_color(role: Role) -> Color {
    match role {
        Role::User => Color::Cyan,
        Role::Assistant => Color::Green,
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

fn history_lines(session: &Session) -> Vec<Line<'static>> {
    session
        .messages
        .iter()
        .flat_map(|m| -> Vec<Line<'static>> {
            let color = role_color(m.role);
            match m.role {
                Role::Tool => {
                    let sanitized = sanitize(&format_tool_result(&m.content));
                    sanitized
                        .split('\n')
                        .map(|line| {
                            Line::from(Span::styled(line.to_string(), Style::default().fg(color)))
                        })
                        .collect()
                }
                Role::User | Role::Assistant => {
                    let mut lines = Vec::new();
                    if !m.tool_calls.is_empty() {
                        for call_line in format_tool_calls(&m.tool_calls) {
                            lines.push(Line::from(Span::styled(
                                sanitize(&call_line),
                                Style::default().fg(color),
                            )));
                        }
                    }
                    if !m.content.is_empty() {
                        let sanitized = sanitize(&m.content);
                        let prefix = format!("{}: ", label(m.role));
                        let mut in_code_block = false;
                        for (i, raw_line) in sanitized.split('\n').enumerate() {
                            if raw_line.trim_start().starts_with("```") {
                                in_code_block = !in_code_block;
                                lines.push(Line::from(Span::styled(
                                    String::new(),
                                    Style::default().bg(Color::DarkGray),
                                )));
                                continue;
                            }
                            let text = if i == 0 {
                                format!("{prefix}{raw_line}")
                            } else {
                                raw_line.to_string()
                            };
                            if in_code_block {
                                lines.push(Line::from(Span::styled(
                                    text,
                                    Style::default().fg(color).bg(Color::DarkGray),
                                )));
                            } else {
                                lines.push(Line::from(format_inline(&text, color)));
                            }
                        }
                    }
                    lines
                }
            }
        })
        .collect()
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

#[allow(clippy::too_many_arguments)]
pub fn render_chat(
    frame: &mut Frame,
    session: &Session,
    input: &str,
    status: &Status,
    header: &HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
    local_lines: &[Line<'static>],
) {
    let area = frame.area();
    // Zero height when there's nothing to show, so the layout collapses
    // back to the plain split the moment the input stops starting with
    // `/` — this row only exists while it has content.
    let suggestions_height = if suggestions.is_empty() {
        0
    } else {
        suggestions.len() as u16 + 1
    };
    let [
        header_area,
        history_area,
        status_area,
        suggestions_area,
        input_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(7),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(suggestions_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // A bordered header box shaped after codex's own: a dim border, a
    // bold title line, then dim-labeled `model:`/`directory:`/`tokens:`
    // rows with their labels padded to the same width so the values line
    // up in a column, matching codex's own layout without reusing its
    // wording — polaris has no interactive model picker, so this states
    // the provider next to the model name instead.
    let dim = Style::default().add_modifier(Modifier::DIM);
    let header_lines = vec![
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
    ];
    frame.render_widget(
        Paragraph::new(header_lines)
            .block(Block::default().borders(Borders::ALL).border_style(dim)),
        header_area,
    );

    // `local_lines` are appended after the real conversation — output from
    // a slash command like `/diff` that's shown to the user but never
    // touches `session.messages` or the model. Sharing this Paragraph and
    // its scroll-to-bottom logic with the real history means it doesn't
    // need its own layout region or its own "is there more than fits"
    // math.
    let mut lines = history_lines(session);
    lines.extend(local_lines.iter().cloned());
    // Pin the view to the newest messages: once there are more lines than
    // fit, scroll so the last line lands on the last visible row. Without
    // this, a Paragraph always renders from line 0 and the most recent
    // reply — the thing a chat screen exists to show — scrolls off the
    // bottom out of view as the conversation grows.
    //
    // The block below reserves a top border row, so the actual visible
    // text area is one row shorter than `history_area` itself.
    let visible_rows = history_area.height.saturating_sub(1) as usize;
    let scroll_offset = lines.len().saturating_sub(visible_rows) as u16;

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::TOP))
            .scroll((scroll_offset, 0)),
        history_area,
    );

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
        // full name is still possible but no longer required.
        let lines: Vec<Line> = suggestions
            .iter()
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
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::TOP)),
            suggestions_area,
        );
    }

    // No box around the input line — a bare `\u{203a} ` prompt, matching
    // codex's own composer, with a dim placeholder while empty instead of
    // an empty bordered box.
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
    frame.render_widget(Paragraph::new(input_line), input_area);

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

    #[test]
    fn a_user_message_appears_with_its_role_label() {
        let mut session = Session::default();
        session.push_user("Cargo.toml は何行か");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("you"));
        assert!(content.contains("Cargo.toml"));
    }

    #[test]
    fn the_status_line_shows_thinking_while_a_turn_is_in_flight() {
        let session = Session::default();
        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let status = Status::Thinking {
            elapsed: Duration::from_secs(3),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &status, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("Working"));
        assert!(content.contains("3s"));
        assert!(content.contains("esc to interrupt"));
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
    fn history_longer_than_the_screen_scrolls_to_show_the_newest_message() {
        let mut session = Session::default();
        for i in 0..30 {
            session.push_user(&format!("message number {i}"));
        }

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("message number 29"));
        assert!(!content.contains("message number 0 "));
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

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!content.chars().any(|c| c == '\u{1b}'));
        // The rest of the text should still be visible, just with the
        // control bytes neutralized rather than the whole message dropped.
        assert!(content.contains("fake red"));
    }

    #[test]
    fn a_tool_result_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
        let mut session = Session::default();
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo fake"}),
            }],
        );
        session.push_tool_result("c1", "\x1b[31mfake\x1b[0m");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!content.chars().any(|c| c == '\u{1b}'));
        // The rest of the text should still be visible, just with the
        // control bytes neutralized rather than the whole tool result
        // being dropped.
        assert!(content.contains("fake"));
    }

    #[test]
    fn ordinary_text_renders_unaffected_by_sanitization() {
        let mut session = Session::default();
        session.push_user("plain ascii and 日本語 text, nothing weird here.");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
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
        let session = Session::default();
        let status = Status::Error("\x1b[31mfake\x1b[0m".to_string());

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &status, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!content.chars().any(|c| c == '\u{1b}'));
        assert!(content.contains("error: "));
        assert!(content.contains("fake"));
    }

    #[test]
    fn a_multi_line_reply_renders_as_multiple_lines_not_one_clipped_line() {
        let mut session = Session::default();
        session.push_assistant("first paragraph\nsecond paragraph\nthird paragraph");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        // Every physical line of the message must land on its own row of
        // the rendered buffer, not be squashed onto a single row.
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
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
        let session = Session::default();
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
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("openai"));
        assert!(content.contains("gpt-5.4"));
        assert!(content.contains("140"));
    }

    #[test]
    fn the_footer_shows_the_model_and_effort_together() {
        let session = Session::default();
        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "high",
            provider_name: "openai",
            model_name: "gpt-5.6-sol",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        // Matches the verified screenshot's footer shape: "{model}
        // {effort} · {cwd}".
        assert!(content.contains("gpt-5.6-sol high"));
        assert!(content.contains("~/example"));
    }

    #[test]
    fn a_tool_call_and_its_result_are_shown_in_the_history() {
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
        session.push_tool_result("c1", "hello\n");
        session.push_assistant("a.txt contains \"hello\"");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            content.contains("read"),
            "the tool name should appear: {content}"
        );
        assert!(
            content.contains("hello"),
            "the tool result should appear: {content}"
        );
    }

    #[test]
    fn a_multi_line_tool_result_renders_as_multiple_lines_not_one_clipped_line() {
        let mut session = Session::default();
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
        );
        // Mimics `read`'s real output shape: "{line_number}\t{content}\n"
        // per line.
        session.push_tool_result("c1", "1\tfirst line\n2\tsecond line\n3\tthird line\n");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        // Each physical line of the tool result must land on its own row,
        // not be squashed onto a single row.
        assert!(rows.iter().any(|r| r.contains("first line")));
        assert!(rows.iter().any(|r| r.contains("second line")));
        assert!(rows.iter().any(|r| r.contains("third line")));
        // Tabs must be expanded to spaces, not rendered as literal tab
        // characters (which are invisible in a terminal grid).
        let full: String = rows.join("\n");
        assert!(!full.contains('\t'));
        assert!(rows.iter().any(|r| r.contains("1    first line")));
    }

    #[test]
    fn a_long_tool_result_is_truncated_in_the_display() {
        let mut session = Session::default();
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "big.txt"}),
            }],
        );
        let long_body: String = "x".repeat(500);
        session.push_tool_result("c1", &long_body);

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        // The full 500-character body must not appear verbatim; only a
        // prefix of it should.
        assert!(!content.contains(&long_body));
    }

    #[test]
    fn bold_text_is_rendered_with_the_bold_modifier() {
        let mut session = Session::default();
        session.push_assistant("this is **bold** text");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let bold_cell = (0..buffer.area.width)
            .flat_map(|x| (0..buffer.area.height).map(move |y| (x, y)))
            .find(|&(x, y)| {
                buffer[(x, y)].symbol() == "b" && {
                    let row: String = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    row.contains("bold")
                }
            });
        let (x, y) = bold_cell.expect("the word 'bold' should appear somewhere");
        assert!(
            buffer[(x, y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD),
            "the 'b' in 'bold' should carry the BOLD modifier"
        );
    }

    #[test]
    fn inline_code_and_surrounding_text_both_render_without_the_backticks() {
        let mut session = Session::default();
        session.push_assistant("run `cargo test` now");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("cargo test"));
        assert!(!content.contains('`'));
    }

    #[test]
    fn user_and_assistant_lines_use_different_colors() {
        let mut session = Session::default();
        session.push_user("hello");
        session.push_assistant("hi there");

        let header = HeaderInfo {
            cwd: "/tmp/example",
            cwd_short: "~/example",
            effort_name: "low",
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header, &[], 0, &[]))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let find_row_color = |needle: &str| {
            for y in 0..buffer.area.height {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if row.contains(needle) {
                    return buffer[(0, y)].fg;
                }
            }
            panic!("row containing {needle:?} not found");
        };
        assert_ne!(find_row_color("hello"), find_row_color("hi there"));
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
