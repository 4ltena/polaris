//! Pure rendering: turns a `Session` + input state into terminal cells.
//! Kept free of any actual terminal I/O so it's testable with
//! `ratatui::backend::TestBackend`.

use std::time::Duration;

use polaris_core::{AgentEvent, DiffLine};
use polaris_provider::Role;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Turns a tool call's JSON `arguments` string into a compact activity label.
/// File operations expose only their target path; their ranges and contents
/// belong in the result/diff, not in the activity heading. `spawn` similarly
/// exposes only declared agent types and never repeats model-authored task
/// prose. Other tools retain the stable `key=value` preview.
fn format_tool_call_args(name: &str, detail: &str) -> String {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(detail) else {
        return detail.to_string();
    };
    if matches!(name, "read" | "write" | "edit")
        && let Some(path) = map.get("path").and_then(serde_json::Value::as_str)
    {
        return path.to_string();
    }
    if name == "spawn"
        && let Some(tasks) = map.get("tasks").and_then(serde_json::Value::as_array)
    {
        return tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                let agent_type = task
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("auto");
                format!(
                    "{} ({agent_type})",
                    polaris_core::desktop_store::child_display_name(index)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
    }
    map.iter()
        .map(|(k, v)| format!("{k}={}", serde_json::to_string(v).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The color code (tool result previews, markdown-fenced code blocks in
/// replies) is shown in — distinguishes it visually from ordinary text
/// without a background tint, which read as unwanted shading when
/// applied to arbitrary command output, e.g. a `bash` "git commit", or
/// to a whole fenced block in a reply.
const CODE_TEXT_COLOR: Color = Color::Rgb(0x98, 0x9d, 0xcc);

/// Background color for an active/finalized mouse selection. Terminal
/// cells have no real alpha channel, so "more transparent" is simulated by
/// blending Tailwind's `blue-400` (`#60A5FA`) toward black — currently at
/// ~45% of its original brightness, a muted blue that reads as a tint over
/// the content rather than a solid opaque highlight. Applied as a real
/// background color rather than `Modifier::REVERSED`, so it reads the same
/// regardless of the terminal's own color scheme.
pub(crate) const SELECTION_HIGHLIGHT_COLOR: Color = Color::Rgb(0x2b, 0x4a, 0x71);

/// The most diff lines (context/added/removed, combined across all hunks)
/// shown live for one `ToolFinished { diff: Some(_), .. }` event, before the
/// rest is elided with a notice row — the live-print analog of
/// `TOOL_RESULT_PREVIEW_CHARS`, bounding how much screen a single write/edit
/// can claim mid-turn.
const MAX_DIFF_LINES_SHOWN: usize = 40;

/// How many rows one tool-result preview may claim at most.
const TOOL_RESULT_PREVIEW_LINES: usize = 5;

/// A tool result's live preview, already sanitized and split into rows.
/// Empty when the result carries nothing worth a row (an empty body, or a
/// body of nothing but whitespace).
///
/// The truncation matches the batch-print era's `format_tool_result`: tabs
/// expanded first so the budget is spent on what is actually rendered, then
/// `TOOL_RESULT_PREVIEW_CHARS` characters, then `...` if anything was cut.
/// Rows are capped at `TOOL_RESULT_PREVIEW_LINES` for the same reason they
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
    let shown = all.len().min(TOOL_RESULT_PREVIEW_LINES);
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
        AgentEvent::MemoryPrepared(observation) => observation
            .display_lines()
            .into_iter()
            .map(|line| {
                HistoryLine::plain(Line::from(Span::styled(
                    sanitize(&line),
                    Style::default().add_modifier(Modifier::DIM),
                )))
            })
            .collect(),
        // Desktop-only provisional text is replaced by the authoritative
        // completion. The legacy TUI continues to render that completion once.
        // Model request boundaries are host bookkeeping, not transcript.
        AgentEvent::TextDelta { .. }
        | AgentEvent::ModelRequestStarted { .. }
        | AgentEvent::ModelRequestFinished { .. }
        | AgentEvent::DecisionStarted
        | AgentEvent::DecisionFinished
        | AgentEvent::EffortJudging
        | AgentEvent::EffortJudged { .. } => Vec::new(),
        AgentEvent::WorkflowResolved {
            phase,
            skill_ids,
            changed_skill_ids,
            manifest_hash,
        } => {
            let selected = if skill_ids.is_empty() {
                "(none)".to_string()
            } else {
                skill_ids.join(", ")
            };
            let mut text =
                format!("workflow phase={phase}; skills={selected}; manifest={manifest_hash}");
            if !changed_skill_ids.is_empty() {
                text.push_str(&format!("; changed={}", changed_skill_ids.join(", ")));
            }
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&text),
                Style::default().add_modifier(Modifier::DIM),
            )))]
        }
        AgentEvent::ToolStarted { name, detail } => {
            let args = format_tool_call_args(name, detail);
            let preview: String = args.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
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
                    Style::default().fg(CODE_TEXT_COLOR),
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
                                "  ...(omitted)",
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
        AgentEvent::SpawnStarted {
            display_name,
            agent_type,
            task: _,
        } => vec![
            HistoryLine::plain(Line::from(Span::raw(sanitize(&format!(
                "⏺ {display_name} ({agent_type})"
            ))))),
            HistoryLine::plain(Line::from(Span::styled(
                "  → starting",
                Style::default().add_modifier(Modifier::DIM),
            ))),
        ],
        AgentEvent::SpawnActivity { name, detail, .. } => {
            let args = format_tool_call_args(name, detail);
            let preview: String = args.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!("  → {name}({preview})")),
                Style::default().add_modifier(Modifier::DIM),
            )))]
        }
        AgentEvent::SpawnFinished {
            display_name,
            agent_type,
            ok,
        } => {
            let marker = if *ok { "done" } else { "failed" };
            let marker_color = if *ok { Color::Green } else { Color::Red };
            vec![HistoryLine::plain(Line::from(vec![
                Span::styled("⏺ ", Style::default().fg(marker_color)),
                Span::styled(
                    sanitize(&format!("{display_name} ({agent_type}): {marker}")),
                    Style::default().fg(Color::White),
                ),
            ]))]
        }
        AgentEvent::HistoryCompacted {
            messages_before,
            messages_after,
            tokens_before,
            tokens_after,
        } => {
            vec![HistoryLine::plain(Line::from(Span::styled(
                sanitize(&format!(
                    "⏺ Conversation history summarized ({messages_before}→{messages_after} messages, {tokens_before}→{tokens_after} tokens)"
                )),
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
                    base_style.fg(CODE_TEXT_COLOR),
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
#[derive(Clone)]
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
/// Recognizes a block-level `#`-`######` heading or a `-`/`*` bullet at the
/// start of a line and returns the line with its marker replaced/stripped,
/// plus whether it was a heading (the caller bolds the whole line for a
/// heading; a bullet's substituted `•` needs no extra styling). Only
/// called outside a fenced code block — a `#`/`-` inside one is literal
/// text, not markdown.
fn strip_block_markdown(raw_line: &str) -> (String, bool) {
    let trimmed = raw_line.trim_start();
    let indent = &raw_line[..raw_line.len() - trimmed.len()];

    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return (format!("{indent}\u{2022} {rest}"), false);
    }

    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes)
        && let Some(rest) = trimmed[hashes..].strip_prefix(' ')
    {
        return (rest.to_string(), true);
    }

    (raw_line.to_string(), false)
}

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
                                    line: Line::from(String::new()),
                                    shaded: false,
                                });
                                continue;
                            }
                            if in_code_block {
                                let text = if i == 0 {
                                    format!("{prefix}{raw_line}")
                                } else {
                                    raw_line.to_string()
                                };
                                lines.push(HistoryLine {
                                    line: Line::from(Span::styled(
                                        text,
                                        Style::default().fg(CODE_TEXT_COLOR),
                                    )),
                                    shaded: false,
                                });
                            } else {
                                let (body, is_heading) = strip_block_markdown(raw_line);
                                let text = if i == 0 {
                                    format!("{prefix}{body}")
                                } else {
                                    body
                                };
                                let mut spans = format_inline(&text, color);
                                if is_heading {
                                    for span in &mut spans {
                                        span.style = span.style.add_modifier(Modifier::BOLD);
                                    }
                                }
                                lines.push(HistoryLine {
                                    line: Line::from(spans),
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

/// Moves `current` into `rows`, first dropping any trailing spaces —
/// they were speculatively attached to `current` by the wrap loop below
/// in case more content landed on the same row; once a row is actually
/// being closed out, they'd only show up as invisible-but-present
/// trailing whitespace.
fn push_row_trimmed(rows: &mut Vec<Vec<(char, Style)>>, current: &mut Vec<(char, Style)>) {
    while matches!(current.last(), Some((' ', _))) {
        current.pop();
    }
    rows.push(std::mem::take(current));
}

/// Reflows any `HistoryLine` whose display width exceeds `width` into
/// several `HistoryLine`s, so a narrow terminal shows the full text across
/// multiple rows instead of `render_history_into` clipping it at the
/// window's right edge. Breaks at whitespace where possible (the space
/// itself is dropped at the break rather than carried to the next row's
/// start); a single word longer than `width` is hard-broken mid-word as a
/// fallback. Per-span styling (bold, dim, background) is preserved
/// exactly, split at wherever a row boundary lands inside it, and every
/// wrapped row inherits the original line's `shaded` flag.
///
/// `HistoryWrapCache` keeps this result between redraws and invalidates it
/// when the width, source length, or an in-place activity row changes. The
/// full result is still needed because a line's wrapped row count is not
/// known until reflow, but scrolling no longer repeats that work.
///
/// `width` of `0` is a no-op (nothing can fit) — callers should not call
/// this with a zero-width area to begin with.
pub fn wrap_history_lines(history: &[HistoryLine], width: u16) -> Vec<HistoryLine> {
    let width = width as usize;
    if width == 0 {
        return history.to_vec();
    }

    let mut out = Vec::with_capacity(history.len());
    for hl in history {
        let total_width: usize = hl.line.spans.iter().map(|s| s.content.width()).sum();
        if total_width <= width {
            out.push(HistoryLine {
                line: hl.line.clone(),
                shaded: hl.shaded,
            });
            continue;
        }

        let chars: Vec<(char, Style)> = hl
            .line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();

        let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
        let mut current: Vec<(char, Style)> = Vec::new();
        let mut current_width = 0usize;
        let mut i = 0;
        while i < chars.len() {
            let word_start = i;
            while i < chars.len() && chars[i].0 != ' ' {
                i += 1;
            }
            let word_end = i;
            let space_start = i;
            while i < chars.len() && chars[i].0 == ' ' {
                i += 1;
            }
            let space_end = i;

            let word_width: usize = chars[word_start..word_end]
                .iter()
                .map(|(c, _)| c.width().unwrap_or(0))
                .sum();

            if word_width > width {
                // The word alone can't fit even on an empty row — hard-break
                // it character by character rather than looping forever.
                for &(c, style) in &chars[word_start..word_end] {
                    let cw = c.width().unwrap_or(0);
                    if current_width + cw > width && !current.is_empty() {
                        push_row_trimmed(&mut rows, &mut current);
                        current_width = 0;
                    }
                    current.push((c, style));
                    current_width += cw;
                }
            } else {
                if current_width + word_width > width && !current.is_empty() {
                    push_row_trimmed(&mut rows, &mut current);
                    current_width = 0;
                }
                current.extend(chars[word_start..word_end].iter().copied());
                current_width += word_width;
            }

            // Trailing spaces travel with the row they fit on; a space that
            // would push past `width` is simply dropped rather than
            // starting the next row with leading whitespace.
            for &(c, style) in &chars[space_start..space_end] {
                let cw = c.width().unwrap_or(0);
                if current_width + cw > width {
                    break;
                }
                current.push((c, style));
                current_width += cw;
            }
        }
        if !current.is_empty() || rows.is_empty() {
            push_row_trimmed(&mut rows, &mut current);
        }

        for row in rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut buf_style: Option<Style> = None;
            for (c, style) in row {
                if buf_style != Some(style) {
                    if !buf.is_empty() {
                        spans.push(Span::styled(
                            std::mem::take(&mut buf),
                            buf_style.unwrap_or_default(),
                        ));
                    }
                    buf_style = Some(style);
                }
                buf.push(c);
            }
            if !buf.is_empty() {
                spans.push(Span::styled(buf, buf_style.unwrap_or_default()));
            }
            out.push(HistoryLine {
                line: Line::from(spans),
                shaded: hl.shaded,
            });
        }
    }
    out
}

/// Cached full-history reflow. Most TUI redraws change only the scroll offset,
/// input buffer, or status animation, so rebuilding every wrapped row on each
/// frame makes navigation cost grow with the entire conversation.
#[derive(Default)]
pub struct HistoryWrapCache {
    width: u16,
    source_len: usize,
    valid: bool,
    wrapped: Vec<HistoryLine>,
    #[cfg(test)]
    rebuilds: usize,
}

impl HistoryWrapCache {
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// Draw a temporary suffix without reflowing the unchanged conversation.
    /// A resize may reflow once at the new width; afterward the cached prefix
    /// remains usable even though the preview is removed from `history`.
    pub(crate) fn with_appended<R>(
        &mut self,
        history: &mut Vec<HistoryLine>,
        appended: Vec<HistoryLine>,
        draw: impl FnOnce(&[HistoryLine], &mut Self) -> R,
    ) -> R {
        let original_len = history.len();
        history.extend(appended);
        if self.valid && self.source_len == original_len {
            self.wrapped
                .extend(wrap_history_lines(&history[original_len..], self.width));
            self.source_len = history.len();
        }
        let result = draw(history, self);
        if self.valid && self.source_len == history.len() {
            let preview_rows = wrap_history_lines(&history[original_len..], self.width).len();
            self.wrapped.truncate(self.wrapped.len() - preview_rows);
            self.source_len = original_len;
        } else {
            self.invalidate();
        }
        history.truncate(original_len);
        result
    }

    pub fn lines<'a>(&'a mut self, history: &[HistoryLine], width: u16) -> &'a [HistoryLine] {
        if !self.valid || self.width != width || self.source_len != history.len() {
            self.wrapped = wrap_history_lines(history, width);
            self.width = width;
            self.source_len = history.len();
            self.valid = true;
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        &self.wrapped
    }

    #[cfg(test)]
    fn rebuilds(&self) -> usize {
        self.rebuilds
    }
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
        // fill (`buf.set_style` over the render area). There's no
        // `Terminal::insert_before`/scrollback round-trip to lose the fill
        // to anymore — this buffer is the self-managed `Viewport::Fullscreen`
        // frame itself (see `lib.rs`), diffed and flushed to the real
        // terminal fresh every frame — but the explicit per-cell `bg` set
        // here is cheap and was kept as a guard against `Paragraph`'s own
        // fill not covering the full row width in some terminal/backend
        // combination.
        if hl.shaded {
            for x in row.x..row.x + row.width {
                buf[(x, row.y)].set_bg(Color::DarkGray);
            }
        }
    }
}

/// Overlays a selection's highlight onto an already-rendered `Buffer` —
/// call this immediately after `render_history_into` has drawn `lines`
/// into `area`. Sets `SELECTION_HIGHLIGHT_COLOR` as the background; never
/// touches `fg`, so existing foreground colors (role color,
/// `CODE_TEXT_COLOR`) show through unchanged on top of the highlight.
/// `wrapped`/`window` are the same values `draw_frame` already computed
/// for `render_history_into` itself.
pub fn apply_selection_highlight(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    window: std::ops::Range<usize>,
    wrapped: &[HistoryLine],
    sel: &crate::selection::Selection,
) {
    for (line_idx, start_col, end_col) in crate::selection::highlighted_columns(wrapped, sel) {
        if !window.contains(&line_idx) {
            continue;
        }
        let y_offset = (line_idx - window.start) as u16;
        if y_offset >= area.height {
            continue;
        }
        let row_y = area.y + y_offset;

        // start_col/end_col are character indices into the line's full
        // text; walk the same way `wrap_history_lines` does to turn them
        // into display columns (a full-width character occupies 2 cells).
        let Some(hl) = wrapped.get(line_idx) else {
            continue;
        };
        let mut char_idx = 0usize;
        let mut display_col: u16 = 0;
        'spans: for span in &hl.line.spans {
            for c in span.content.chars() {
                // `UnicodeWidthChar` is already imported unqualified at the
                // top of render.rs (see its `use unicode_width::{...}`) —
                // call `.width()` directly, matching `wrap_history_lines`'s
                // own style.
                let w = c.width().unwrap_or(0) as u16;
                if char_idx >= start_col && char_idx < end_col {
                    for dx in 0..w {
                        let x = area.x + display_col + dx;
                        if x >= area.x + area.width {
                            break 'spans;
                        }
                        buf[(x, row_y)].set_style(Style::default().bg(SELECTION_HIGHLIGHT_COLOR));
                    }
                }
                display_col += w;
                char_idx += 1;
                if char_idx >= end_col {
                    break 'spans;
                }
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
    /// box's `model:` row doesn't carry it. `/model` changes the model only;
    /// this value is informational and comes from the launch configuration.
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

/// Builds the header box's unbordered content lines — a bold title line,
/// then dim-labeled `model:`/`directory:`/`tokens:` rows (no border; see
/// `header_history_lines` for the bordered version actually seeded into
/// `history`). Shared by `header_history_lines` and by tests that want to
/// check its content directly, without the border framing in the way.
pub fn header_lines(header: &HeaderInfo) -> Vec<Line<'static>> {
    header_lines_with_mode(header, false)
}

fn header_lines_with_mode(header: &HeaderInfo, read_only: bool) -> Vec<Line<'static>> {
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
            Span::styled(
                format!(" v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        if read_only {
            Line::from("mode:      Windows browsing only (/help)")
        } else {
            Line::from(vec![
                Span::styled("model:     ", dim),
                Span::raw(sanitize(header.model_name)),
                Span::raw(format!(" ({})   ", sanitize(header.provider_name))),
                Span::styled("/model", Style::default().fg(Color::Cyan)),
                Span::styled(" to change", dim),
            ])
        },
        Line::from(vec![
            Span::styled("directory: ", dim),
            Span::raw(sanitize(header.cwd)),
        ]),
        if read_only {
            Line::from("session:   not started; no persistence; no model connection")
        } else {
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
            ])
        },
    ]
}

/// `header_lines(header)`, framed with a hand-built box-drawing border, as
/// plain `HistoryLine`s — the header's one-shot equivalent of
/// `history_lines_for`. Used at startup and by every conversation-view
/// reset (see `lib.rs`'s `run()` and `ConversationViewState::reset`) to re-seed
/// `history`; there is no separate `render_header_into`/`insert_before`
/// print anymore — the header lives in `history` like everything else.
/// Always returns exactly `HEADER_HEIGHT` rows.
pub fn header_history_lines(header: &HeaderInfo, width: u16) -> Vec<HistoryLine> {
    header_history_lines_with_mode(header, width, false)
}

pub(crate) fn header_history_lines_with_mode(
    header: &HeaderInfo,
    width: u16,
    read_only: bool,
) -> Vec<HistoryLine> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let w = width as usize;
    let inner = w.saturating_sub(2);

    let top = format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(inner));
    let bottom = format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner));

    let mut out = Vec::with_capacity(HEADER_HEIGHT as usize);
    out.push(HistoryLine::plain(Line::styled(top, dim)));
    for content in header_lines_with_mode(header, read_only) {
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

/// The most suggestion rows shown at once — the footer region has a fixed
/// `Constraint::Length` in `draw_frame`'s (`lib.rs`) vertical layout split
/// and can't grow per-frame to fit content, so a query matching more than
/// this many slash commands shows the first `MAX_DISPLAYED_SUGGESTIONS`
/// plus a "+N more" row instead of growing without bound the way the
/// scrollable history region above it can.
pub const MAX_DISPLAYED_SUGGESTIONS: usize = 8;

/// How many rows the suggestions popup alone needs for `suggestion_count`
/// live matches — `0` when there are none (the popup collapses away
/// entirely), otherwise the shown rows (capped at
/// `MAX_DISPLAYED_SUGGESTIONS`) plus one row for the popup's own top
/// border, plus one more when the match count is truncated (the "+N more,
/// keep typing" row). Factored out of `render_footer` so `footer_height`
/// (below) can share the exact same computation instead of an
/// independently maintained copy that could silently drift from what
/// `render_footer` actually lays out.
fn suggestions_popup_height(suggestion_count: usize) -> u16 {
    if suggestion_count == 0 {
        return 0;
    }
    let shown = suggestion_count.min(MAX_DISPLAYED_SUGGESTIONS);
    let truncated = suggestion_count > MAX_DISPLAYED_SUGGESTIONS;
    shown as u16 + 1 + u16::from(truncated)
}

/// The footer region's total height for `suggestion_count` live
/// suggestions — the dynamic replacement for the old fixed `FOOTER_HEIGHT`
/// constant. `draw_frame` (`lib.rs`) calls this every frame to size its
/// `Constraint::Length` split, so the footer only ever claims as many rows
/// as it actually needs this frame (5, the common case with no popup
/// showing) rather than a worst-case reservation that left a permanent
/// blank gap above it whenever the popup wasn't at its maximum. The same
/// four mouse-event handler sites that independently recompute
/// `history_area`'s height outside the `draw()` closure (see
/// `area_height_for_history` in `lib.rs`) call this too, so both stay in
/// sync with what this frame's `render_footer` call actually renders.
///
/// Always `5 + suggestions_popup_height(suggestion_count)`: status(1) +
/// suggestions popup + input pad-above(1) + input(1) + input
/// pad-below(1) + footer/model-line(1).
pub fn footer_height(suggestion_count: usize) -> u16 {
    5 + suggestions_popup_height(suggestion_count)
}

fn footer_areas(
    area: ratatui::layout::Rect,
    suggestion_count: usize,
) -> [ratatui::layout::Rect; 6] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(suggestions_popup_height(suggestion_count)),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .flex(Flex::End)
    .areas(area)
}

fn composer_cursor_position(
    input_area: ratatui::layout::Rect,
    input: &str,
    cursor: usize,
) -> Position {
    let prefix_width: u16 = 2;
    let typed_width = sanitize(&crate::file_input::displayed_prefix(input, cursor)).width() as u16;
    let cursor_x = input_area
        .x
        .saturating_add(prefix_width)
        .saturating_add(typed_width)
        .min(input_area.right().saturating_sub(1));
    Position::new(cursor_x, input_area.y)
}

/// The composer row and caret position for a completed chat frame.
///
/// `Terminal::draw` may skip this row when its buffer has not changed.  A
/// terminal or IME can still have cleared its unused suffix, so callers use
/// this geometry to resend exactly this row after the normal frame draw.
pub(crate) fn composer_geometry(
    frame_area: ratatui::layout::Rect,
    suggestion_count: usize,
    input: &str,
    cursor: usize,
) -> Option<(ratatui::layout::Rect, Position)> {
    let footer_height = footer_height(suggestion_count);
    let [_, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).areas(frame_area);
    let [_, _, _, input_area, _, _] = footer_areas(footer_area, suggestion_count);
    (input_area.width > 0 && input_area.height > 0).then(|| {
        (
            input_area,
            composer_cursor_position(input_area, input, cursor),
        )
    })
}

/// Everything redrawn every frame: the suggestions popup (while typing a
/// `/command`), the status row (idle / thinking-with-shimmer / error /
/// notice), the input line, and the footer (`model effort · cwd`). This is
/// the footer region of `draw_frame`'s (`lib.rs`) fullscreen layout split —
/// the fixed-height bottom slice below the scrollable history region, which
/// renders `history` separately via `render_history_into`. Replaces the old
/// `render_chat`, which drew the header and full history inline with
/// everything else on every frame; splitting it out is what makes the
/// header/history scrollable as their own region instead of being
/// repainted (and losing scroll position) with the footer every frame.
#[allow(clippy::too_many_arguments)]
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
    render_footer_with_mode(
        frame,
        area,
        input,
        cursor,
        status,
        header,
        suggestions,
        selected_suggestion,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_footer_with_mode(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    input: &str,
    cursor: usize,
    status: &Status,
    header: &HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
    read_only: bool,
) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    // Zero height when there's nothing to show, so the layout collapses
    // back to the plain split the moment the input stops starting with
    // `/` — this row only exists while it has content. Shared with
    // `footer_height` (see its doc) so the two can never disagree about
    // how tall the popup is this frame.
    let shown = suggestions.len().min(MAX_DISPLAYED_SUGGESTIONS);
    // `area` is `draw_frame`'s `footer_height(suggestions.len())`
    // reservation — normally an exact fit for what this frame actually
    // needs, but `Flex::End` is kept regardless: it costs nothing when
    // `area` is already an exact fit, and it's what makes the standalone
    // `the_input_row_reaches_the_true_bottom_of_the_footer_reservation`
    // regression test meaningful (an artificially oversized `area`, to
    // pin down this bottom-anchoring behavior independent of how
    // `draw_frame` sizes the area it passes in). Without it, the default
    // `Flex::Start` would stack these six rows at `area`'s *top* and leave
    // any leftover as blank space below the input box instead of above
    // `status_area`.
    let [
        status_area,
        suggestions_area,
        input_pad_top_area,
        input_area,
        input_pad_bottom_area,
        footer_area,
    ] = footer_areas(area, suggestions.len());

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
        // way the old fullscreen layout could. `window_start` slides the
        // visible page so the selection is always inside it — pinning to
        // the first `shown` items regardless of `selected_suggestion`
        // used to leave the highlight scrolled off-screen once it moved
        // past the first page.
        let window_start = selected_suggestion.saturating_sub(shown.saturating_sub(1));
        let window_end = (window_start + shown).min(suggestions.len());
        let mut lines: Vec<Line> = suggestions[window_start..window_end]
            .iter()
            .enumerate()
            .map(|(local_i, c)| {
                let i = window_start + local_i;
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
        let below = suggestions.len() - window_end;
        if below > 0 {
            lines.push(Line::from(Span::styled(
                format!("  \u{2026} {below} more, keep typing"),
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
    // not just the styled Spans' own cells), matching codex's own
    // input-box styling and how a submitted line looks once it's printed
    // into history (`HistoryLine::shaded`, set for `Role::User`).
    let input_line = if input.is_empty() {
        Line::from(vec![
            Span::styled(
                "\u{203a} ",
                Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::styled(
                if read_only {
                    "Use /help to list browsing commands"
                } else {
                    "Ask polaris to do anything"
                },
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        let mut spans = vec![Span::styled(
            "\u{203a} ",
            Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
        )];
        let mut after = 0;
        for block in crate::file_input::blocks(input) {
            spans.push(Span::raw(sanitize(&input[after..block.range.start])));
            spans.push(Span::styled(
                sanitize(&block.label),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            after = block.range.end;
        }
        spans.push(Span::raw(sanitize(&input[after..])));
        Line::from(spans)
    };
    // Paint the composer as one solid three-row surface before adding its
    // text. Rendering three independent paragraphs allowed a terminal's
    // incremental redraw to retain the default background across the unused
    // suffix of the middle row while typing, visually shortening the field.
    let composer_area = ratatui::layout::Rect::new(
        input_area.x,
        input_pad_top_area.y,
        input_area.width,
        input_pad_top_area
            .height
            .saturating_add(input_area.height)
            .saturating_add(input_pad_bottom_area.height),
    );
    let composer_style = Style::default().bg(Color::DarkGray);
    frame.render_widget(Block::default().style(composer_style), composer_area);
    frame.render_widget(Paragraph::new(input_line).style(composer_style), input_area);
    // A few terminal backends clear the unused suffix of a redrawn Paragraph
    // even though its base style has a background. Patch the final frame
    // buffer after text rendering so typing cannot punch a default-background
    // gap into the right side of the composer.
    frame.buffer_mut().set_style(composer_area, composer_style);

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
    frame.set_cursor_position(composer_cursor_position(input_area, input, cursor));

    // Two-tone footer matching codex's own status line: the model name in
    // a warm tan, the working directory in a soft green, separated by a
    // dim middle dot.
    let model_and_effort = if read_only {
        "Windows browsing only".to_owned()
    } else if header.effort_name.is_empty() {
        sanitize(header.model_name)
    } else {
        format!(
            "{} {}",
            sanitize(header.model_name),
            sanitize(header.effort_name)
        )
    };
    let footer_line = Line::from(vec![
        Span::styled(
            model_and_effort,
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
    let inner_height = area.height.saturating_sub(2) as usize;
    let show_header = inner_height >= 5;
    let show_footer = inner_height >= 2;
    let visible_rows = inner_height
        .saturating_sub(usize::from(show_header) * 2 + usize::from(show_footer))
        .max(1);
    let total = groups.iter().map(|g| g.sessions.len()).sum::<usize>();
    let selected = selected.min(total.saturating_sub(1));
    let mut rows: Vec<Line> = Vec::new();
    let mut selected_row = 0usize;
    let mut selected_group = "";
    let mut idx = 0usize;
    for group in groups {
        rows.push(Line::from(Span::styled(
            sanitize(&group.cwd).escape_debug().to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for s in &group.sessions {
            let is_selected = idx == selected;
            if is_selected {
                selected_row = rows.len();
                selected_group = &group.cwd;
            }
            let marker = if is_selected { "\u{276f} " } else { "  " };
            let when = crate::time::format_relative(s.started_at_millis, now_millis);
            let text = format!(
                "{marker}{when}  {} message(s)  {}",
                s.message_count,
                sanitize(&s.preview).escape_debug()
            );
            let style = if is_selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            rows.push(Line::from(Span::styled(text, style)));
            idx += 1;
        }
    }

    if total == 0 {
        rows.clear();
        rows.push(Line::from(Span::styled("no saved conversations yet", dim)));
    }
    let page_start = selected_row.saturating_sub(visible_rows.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    if show_header {
        lines.push(Line::from(Span::styled(
            "resume a conversation",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            sanitize(selected_group).escape_debug().to_string(),
            dim,
        )));
    }
    lines.extend(rows.into_iter().skip(page_start).take(visible_rows));
    if show_footer {
        lines.push(Line::from(Span::styled(
            "enter to resume \u{b7} esc to cancel",
            dim,
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// A mismatched workspace may inspect a saved conversation, but the viewer
/// never changes the active session or offers a model action. Unlike the
/// ordinary history pane, it includes tool messages and call arguments so
/// the saved transcript can be inspected without opening its raw file.
pub fn transcript_lines_for(messages: &[polaris_provider::Message]) -> Vec<HistoryLine> {
    let mut lines = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "polaris",
            Role::Tool => "tool",
        };
        lines.push(HistoryLine::plain(Line::styled(
            role.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for text in message.content.split('\n') {
            lines.push(HistoryLine::plain(Line::from(sanitize(text))));
        }
        for call in &message.tool_calls {
            lines.push(HistoryLine::plain(Line::from(sanitize(&format!(
                "→ {}({})",
                call.name, call.arguments
            )))));
        }
        lines.push(HistoryLine::plain(Line::from("")));
    }
    lines
}

pub fn render_transcript_viewer(
    frame: &mut Frame,
    lines: &[HistoryLine],
    offset: usize,
    source: &str,
) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title("View conversation (execution unavailable)");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let content_height = inner.height.saturating_sub(2) as usize;
    let start = offset.min(lines.len().saturating_sub(content_height));
    let end = (start + content_height).min(lines.len());
    render_history_into(
        frame.buffer_mut(),
        ratatui::layout::Rect {
            height: content_height as u16,
            ..inner
        },
        &lines[start..end],
    );
    if inner.height >= 2 {
        let footer = ratatui::layout::Rect {
            y: inner.y + inner.height - 2,
            height: 2,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    sanitize(source),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "↑↓/PgUp/PgDn: move · Esc: back",
                    Style::default().fg(Color::DarkGray),
                )),
            ]),
            footer,
        );
    }
}

pub struct RewindPickerRow {
    pub boundary_id: String,
    pub user_message: String,
    pub recorded_at_millis: u128,
}

/// Only verified, complete boundaries are passed here by the session owner.
pub fn render_rewind_picker(
    frame: &mut Frame,
    rows: &[RewindPickerRow],
    selected: usize,
    now_millis: u128,
) {
    render_rewind_picker_with_reason(frame, rows, selected, now_millis, "");
}

pub(crate) fn render_rewind_picker_with_reason(
    frame: &mut Frame,
    rows: &[RewindPickerRow],
    selected: usize,
    now_millis: u128,
    unavailable_reason: &str,
) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![
        Line::from(Span::styled(
            "rewind before a message",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no complete, reversible checkpoint is available",
            dim,
        )));
        if !unavailable_reason.is_empty() {
            lines.push(Line::from(sanitize(unavailable_reason)));
        }
    }
    for (index, row) in rows.iter().enumerate() {
        let marker = if index == selected { "\u{276f} " } else { "  " };
        let when = crate::time::format_relative(row.recorded_at_millis, now_millis);
        let preview = sanitize(&row.user_message.replace('\n', " "));
        let style = if index == selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{when}  {preview}"),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if rows.is_empty() {
            "any key to close"
        } else {
            "enter to preview file and context changes \u{b7} esc to cancel"
        },
        dim,
    )));
    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris"));
    let paragraph = if rows.is_empty() {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, frame.area());
}

pub struct RewindPreviewView {
    pub user_message: String,
    pub changed_paths: Vec<String>,
    pub recovery_location: String,
}

pub fn render_rewind_preview(frame: &mut Frame, preview: &RewindPreviewView) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![
        Line::from(Span::styled(
            "restore files and conversation context",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "Before: {}",
            sanitize(&preview.user_message.replace('\n', " "))
        )),
        Line::from(Span::styled(
            "The working tree will change. Later conversation, workflow and memory state will not enter the new session.",
            dim,
        )),
        Line::from(Span::styled(
            "External effects and commands already run cannot be undone.",
            dim,
        )),
        Line::from(""),
        Line::from(format!("Files to change: {}", preview.changed_paths.len())),
    ];
    if preview.changed_paths.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", dim)));
    } else {
        for path in preview.changed_paths.iter().take(20) {
            lines.push(Line::from(format!("  {}", sanitize(path))));
        }
        if preview.changed_paths.len() > 20 {
            lines.push(Line::from(format!(
                "  ... and {} more",
                preview.changed_paths.len() - 20
            )));
        }
    }
    lines.extend([
        Line::from(""),
        Line::from(format!(
            "Recovery: {}",
            sanitize(&preview.recovery_location)
        )),
        Line::from(Span::styled("press r to restore \u{b7} esc to cancel", dim)),
    ]);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        frame.area(),
    );
}

/// Exact management target and scope must fit before confirmation is enabled.
pub fn render_management_confirmation(
    frame: &mut Frame,
    operation: &str,
    target: &str,
    detail: &str,
) -> bool {
    use unicode_width::UnicodeWidthStr;
    let operation = sanitize(operation).escape_debug().to_string();
    let target = sanitize(target).escape_debug().to_string();
    let detail = sanitize(detail).escape_debug().to_string();
    let columns = frame.area().width.saturating_sub(4) as usize;
    let required = [&operation, &target, &detail]
        .iter()
        .map(|text| (text.width() + columns.saturating_sub(1)) / columns.max(1))
        .sum::<usize>()
        + 7;
    let ready = columns >= 36 && frame.area().height as usize >= required;
    let lines = if ready {
        vec![
            Line::from(operation),
            Line::from("Target:"),
            Line::from(target),
            Line::from(""),
            Line::from(detail),
            Line::from(""),
            Line::from("y: execute / Esc: cancel"),
        ]
    } else {
        vec![
            Line::from("Cannot display the entire target. Widen the terminal."),
            Line::from("Esc: cancel"),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Confirm")),
        frame.area(),
    );
    ready
}

pub struct C10ConfirmView {
    pub title: String,
    pub details: Vec<String>,
}

pub fn render_c10_preview(frame: &mut Frame, view: &C10ConfirmView) {
    let lines: Vec<_> = view
        .details
        .iter()
        .map(|text| HistoryLine::plain(Line::from(sanitize(text))))
        .collect();
    let mut text = vec![Line::from(sanitize(&view.title))];
    text.extend(
        wrap_history_lines(&lines, frame.area().width.saturating_sub(2))
            .into_iter()
            .take(frame.area().height.saturating_sub(4) as usize)
            .map(|row| row.line),
    );
    text.push(Line::from("Final confirmation is shown after preparation"));
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("C10 preview")),
        frame.area(),
    );
}

pub fn render_c10_wait(frame: &mut Frame) {
    frame.render_widget(Paragraph::new("Processing the C10 operation.\nWaiting for send or reconciliation results.\nEsc does not cancel the send.\nCtrl-C exits after processing finishes.")
        .block(Block::default().borders(Borders::ALL).title("C10")), frame.area());
}

/// Reuse history reflow so long hashes remain inspectable on narrow terminals.
/// Confirmation is available only after the last wrapped row is visible.
pub fn render_c10_confirmation(frame: &mut Frame, view: &C10ConfirmView, offset: usize) -> bool {
    let area = frame.area();
    let width = area.width.saturating_sub(2);
    let visible = area.height.saturating_sub(4) as usize;
    let input: Vec<_> = view
        .details
        .iter()
        .map(|text| HistoryLine {
            line: Line::from(sanitize(text)),
            shaded: false,
        })
        .collect();
    let wrapped = wrap_history_lines(&input, width);
    let offset = offset.min(wrapped.len().saturating_sub(1));
    let complete = width > 0 && visible > 0 && offset + visible >= wrapped.len();
    let mut lines = vec![Line::from(sanitize(&view.title))];
    lines.extend(
        wrapped
            .iter()
            .skip(offset)
            .take(visible)
            .map(|row| row.line.clone()),
    );
    lines.push(Line::from(if complete {
        "y: confirm · Esc: cancel · ↑↓: details"
    } else {
        "↑↓: review all details · Esc: cancel"
    }));
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("C10")),
        area,
    );
    complete
}

pub struct SyncConfirmView {
    pub recovering: bool,
    pub project: String,
    pub operation: String,
    pub revision: String,
    pub previous_revision: String,
    pub recovery: String,
    pub changes: Vec<String>,
}

pub fn render_sync_confirmation(frame: &mut Frame, view: &SyncConfirmView) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![
        Line::from(if view.recovering {
            "confirm recovery (C12)"
        } else {
            "confirm shared changes (C12)"
        }),
        Line::from(format!("Project: {}", sanitize(&view.project))),
        Line::from(format!("Operation: {}", sanitize(&view.operation))),
        Line::from(format!("Agreed revision: {}", sanitize(&view.revision))),
        Line::from(format!(
            "Previously applied: {}",
            sanitize(&view.previous_revision)
        )),
        Line::from("Rewind disposition: IrreversibleLocal"),
        Line::from(format!("Recovery: {}", sanitize(&view.recovery))),
        Line::from(format!("Changed files: {}", view.changes.len())),
    ];
    let visible = frame.area().height.saturating_sub(13) as usize;
    lines.extend(
        view.changes
            .iter()
            .take(visible)
            .map(|path| Line::from(sanitize(path))),
    );
    if view.changes.len() > visible {
        lines.push(Line::from(format!(
            "… {} more",
            view.changes.len() - visible
        )));
    }
    lines.push(Line::from(if view.recovering {
        "Join saved evidence only; never apply source changes again."
    } else {
        "Changes affect this project; conversation rewind does not undo them."
    }));
    lines.push(Line::from(Span::styled(
        "press y to confirm · esc to cancel",
        dim,
    )));
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        frame.area(),
    );
}

pub struct WorkflowChangeConfirmView<'a> {
    pub inventory: &'a str,
    pub project_id: &'a str,
    pub session_id: &'a str,
    pub generation: u64,
    pub before_phase: &'a str,
    pub after_phase: &'a str,
    pub before_mode: &'a str,
    pub after_mode: &'a str,
}

pub fn render_workflow_change_confirmation(
    frame: &mut Frame,
    view: &WorkflowChangeConfirmView<'_>,
) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let lines = vec![
        Line::from(Span::styled(
            "confirm workflow change",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Operation: {}", sanitize(view.inventory))),
        Line::from(format!("Project: {}", sanitize(view.project_id))),
        Line::from(format!("Session: {}", sanitize(view.session_id))),
        Line::from(format!("Saved generation: {}", view.generation)),
        Line::from("Rewind disposition: CapturedRestorable"),
        Line::from(format!(
            "Phase: {} -> {}",
            sanitize(view.before_phase),
            sanitize(view.after_phase)
        )),
        Line::from(format!(
            "Mode: {} -> {}",
            sanitize(view.before_mode),
            sanitize(view.after_mode)
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Local session change; eligible rewind boundaries restore workflow state.",
            dim,
        )),
        Line::from(Span::styled("press y to save · esc to cancel", dim)),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        frame.area(),
    );
}

/// Fixed hosted choices for `/model`. Local runtimes are discovered only
/// after the separate local-model row is chosen.
pub const MODEL_CATALOG: &[&str] = &["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"];
pub const RECOMMENDED_MODEL: &str = "gpt-6.1-sol";
pub const LOCAL_MODEL_LABEL: &str = "Local model";

/// The `/model` picker: numbered options, arrow-key navigable, the
/// currently-active one marked `(current)` and Sol marked
/// `(recommended)` — the same layout
/// `render_permissions_picker` uses, both modeled on a screenshot of
/// codex's real `/model` picker. Unlike codex's, this carries no
/// per-model description text: codex's descriptions ("Latest frontier
/// agentic coding model", ...) are claims about that specific catalog
/// that polaris has no way to verify for whatever endpoint
/// `POLARIS_BASE_URL` actually points at, so making the same claims here
/// would be asserting something unverified.
pub fn render_model_picker(
    frame: &mut Frame,
    current: &str,
    current_is_local: bool,
    selected: usize,
) {
    let area = frame.area();
    let inner_height = area.height.saturating_sub(2) as usize;
    let show_header = inner_height >= 5;
    let show_footer = inner_height >= 2;
    let local_index = MODEL_CATALOG.len();
    let show_gaps = inner_height
        >= usize::from(show_header) * 2 + local_index + 1 + usize::from(show_footer) + 2;
    let visible_rows = inner_height
        .saturating_sub(
            usize::from(show_header) * 2 + usize::from(show_footer) + usize::from(show_gaps) * 2,
        )
        .max(1);
    let selected = selected.min(local_index);
    let page_start = selected / visible_rows * visible_rows;
    let mut lines: Vec<Line> = Vec::new();
    if show_header {
        lines.push(Line::styled(
            "choose what model to use",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(""));
    }
    for (i, name) in MODEL_CATALOG
        .iter()
        .enumerate()
        .skip(page_start)
        .take(visible_rows)
    {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let recommended = *name == RECOMMENDED_MODEL;
        let is_current = !current_is_local && *name == current;
        let suffix = match (recommended, is_current) {
            (true, true) => " (recommended, current)",
            (true, false) => " (recommended)",
            (false, true) => " (current)",
            (false, false) => "",
        };
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
    if (page_start..page_start + visible_rows).contains(&local_index) {
        if show_gaps {
            lines.push(Line::from(""));
        }
        let local_selected = selected == local_index;
        let local_marker = if local_selected { "\u{276f} " } else { "  " };
        let local_suffix = if current_is_local { " (current)" } else { "" };
        let local_style = if local_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::raw(local_marker),
            Span::styled(
                format!("{}. {}{local_suffix}", local_index + 1, LOCAL_MODEL_LABEL),
                local_style,
            ),
        ]));
    }
    if show_gaps {
        lines.push(Line::from(""));
    }
    if show_footer {
        lines.push(Line::styled(
            "enter to confirm \u{b7} esc to cancel",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// Second-level picker populated only from connected loopback runtimes.
pub fn render_local_model_picker(
    frame: &mut Frame,
    models: &[String],
    current: Option<&str>,
    selected: usize,
) {
    let area = frame.area();
    let inner_height = area.height.saturating_sub(2) as usize;
    let show_header = inner_height >= 5;
    let show_footer = inner_height >= 2;
    let visible_rows = inner_height
        .saturating_sub(usize::from(show_header) * 2 + usize::from(show_footer))
        .max(1);
    let selected = selected.min(models.len().saturating_sub(1));
    let page_start = selected / visible_rows * visible_rows;
    let mut lines: Vec<Line> = Vec::new();
    if show_header {
        lines.push(Line::styled(
            "choose a connected local model",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(""));
    }
    if models.is_empty() {
        lines.push(Line::styled(
            "no connected local models",
            Style::default().add_modifier(Modifier::DIM),
        ));
    } else {
        for (i, model) in models
            .iter()
            .enumerate()
            .skip(page_start)
            .take(visible_rows)
        {
            let is_selected = i == selected;
            let marker = if is_selected { "\u{276f} " } else { "  " };
            let suffix = if current == Some(model.as_str()) {
                " (current)"
            } else {
                ""
            };
            let style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    format!("{}. {}{suffix}", i + 1, sanitize(model).escape_debug()),
                    style,
                ),
            ]));
        }
    }
    if show_footer {
        lines.push(Line::styled(
            "enter to confirm \u{b7} esc to go back",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("polaris")),
        area,
    );
}

/// Human-readable form of the CLI default. `/model` no longer exposes an
/// effort picker: automatic routing owns effort, while `--effort` remains
/// available for fixed noninteractive measurements.
pub const DEFAULT_EFFORT: &str = "extra high";

/// Maps the footer's human-readable default to the literal CLI/API token.
pub fn effort_wire_value(name: &str) -> &str {
    match name {
        "extra high" => "xhigh",
        other => other,
    }
}

/// The inverse of [`effort_wire_value`] for the footer.
pub fn effort_display_name(wire: &str) -> &str {
    match wire {
        "xhigh" => "extra high",
        other => other,
    }
}

/// The `/skills` picker: every skill discovered in this project, browsable
/// (arrow keys move the highlight) but not actionable — polaris has no
/// enable/disable concept for a skill the way codex's own `/skills` →
/// "Enable/Disable Skills" screen does, so Enter and Esc both just close
/// it. Still worth its own full-screen view rather than the one-line,
/// six-name-then-"+N more" `Status::Notice` it replaces: with more than a
/// handful of skills, that line either got truncated or just didn't fit.
/// The selected skill's page stays visible on short terminals.
pub fn render_skills_picker(frame: &mut Frame, skills: &[polaris_skills::Skill], selected: usize) {
    let area = frame.area();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let inner_height = area.height.saturating_sub(2) as usize;
    let show_header = inner_height >= 4;
    let show_footer = inner_height >= 6;
    let skill_rows = inner_height - usize::from(show_header) * 2 - usize::from(show_footer) * 2;
    let page_size = skill_rows.div_ceil(2).max(1);
    let selected = selected.min(skills.len().saturating_sub(1));
    let page_start = selected / page_size * page_size;
    let mut lines: Vec<Line> = Vec::new();

    if show_header {
        let heading = if skills.is_empty() {
            "skills discovered in this project".to_string()
        } else {
            format!(
                "skills discovered in this project ({}/{})",
                selected + 1,
                skills.len()
            )
        };
        lines.push(Line::styled(
            heading,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(""));
    }

    if skills.is_empty() && skill_rows > 0 {
        lines.push(Line::styled("no skills discovered", dim));
    }

    for (row, (i, s)) in skills
        .iter()
        .enumerate()
        .skip(page_start)
        .take(page_size)
        .enumerate()
    {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let name_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        if row * 2 < skill_rows {
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(sanitize(&s.name), name_style),
            ]));
        }
        if row * 2 + 1 < skill_rows {
            lines.push(Line::styled(
                format!("     {}", sanitize(&s.description)),
                dim,
            ));
        }
    }

    if show_footer {
        lines.push(Line::from(""));
        lines.push(Line::styled("enter or esc to close", dim));
    }

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

pub(crate) fn approval_cli_value(policy: polaris_core::approval::ApprovalPolicy) -> &'static str {
    use polaris_core::approval::ApprovalPolicy;
    match policy {
        ApprovalPolicy::Never => "never",
        ApprovalPolicy::OnRequest => "on-request",
        ApprovalPolicy::Always => "always",
    }
}

pub(crate) const PERMISSIONS_MIN_WIDTH: u16 = 40;
pub(crate) const PERMISSIONS_MIN_HEIGHT: u16 = 8;

/// The `/permissions` picker: three numbered options, arrow-key
/// navigable, the currently-active one marked `(current)` — mirroring
/// the numbered/highlighted layout codex's own `/model` picker uses
/// (verified against a screenshot of it), applied here to something
/// polaris already has state for (`ApprovalPolicy`) rather than the
/// model list polaris can't actually switch mid-session yet.
pub fn render_permissions_picker(
    frame: &mut Frame,
    current: polaris_core::approval::ApprovalPolicy,
    ceiling: polaris_core::approval::ApprovalPolicy,
    sandbox: &polaris_sandbox::SandboxPolicy,
    selected: usize,
) {
    let area = frame.area();
    if area.width < PERMISSIONS_MIN_WIDTH || area.height < PERMISSIONS_MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("resize terminal to at least 40 columns and 8 rows; esc to cancel")
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("polaris")),
            area,
        );
        return;
    }
    let compact = area.width < 80 || area.height < 14;
    let sandbox_display = sanitize(&sandbox.describe()).escape_debug().to_string();
    let mut lines: Vec<Line> = if compact {
        vec![
            Line::from(format!(
                "startup --approval {}",
                approval_cli_value(ceiling)
            )),
            Line::from(format!(
                "startup --sandbox {}: {sandbox_display}",
                sandbox.mode().as_str()
            )),
        ]
    } else {
        vec![Line::styled(
            "choose an approval preference within startup limits",
            Style::default().add_modifier(Modifier::BOLD),
        )]
    };
    for (i, (policy, name, description)) in PERMISSIONS_OPTIONS.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "\u{276f} " } else { "  " };
        let suffix = if *policy == current { " (current)" } else { "" };
        let restart = if crate::approval_within_startup_ceiling(ceiling, *policy) {
            String::new()
        } else {
            format!(
                " (unavailable; restart with --approval {})",
                approval_cli_value(*policy)
            )
        };
        let title_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let label = if compact && !crate::approval_within_startup_ceiling(ceiling, *policy) {
            format!(
                "{}. --approval {} (restart)",
                i + 1,
                approval_cli_value(*policy)
            )
        } else {
            format!(
                "{}. {name}{suffix}{}",
                i + 1,
                if compact { "" } else { restart.as_str() }
            )
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(label, title_style),
        ]));
        if !compact {
            lines.push(Line::styled(
                format!("     {description}"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    }
    if !compact {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "startup ceiling: --approval {}",
            approval_cli_value(ceiling)
        )));
        lines.push(Line::from(format!("sandbox fixed: {sandbox_display}")));
        lines.push(Line::from(format!(
            "startup flag: --sandbox {} (other modes need restart)",
            sandbox.mode().as_str()
        )));
    }
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
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("approve?")),
        area,
    );
}

#[cfg(test)]
#[path = "render/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "render/memory_observation_tests.rs"]
mod memory_observation_tests;
