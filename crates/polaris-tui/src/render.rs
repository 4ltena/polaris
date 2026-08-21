//! Pure rendering: turns a `Session` + input state into terminal cells.
//! Kept free of any actual terminal I/O so it's testable with
//! `ratatui::backend::TestBackend`.

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
    Thinking,
    Error(String),
}

/// Replace ASCII control characters that could smuggle terminal escape /
/// OSC sequences (most importantly ESC, 0x1B) into the real terminal once
/// this renders through crossterm. `\n` and `\t` are left alone since they
/// don't start escape sequences and are needed for readable text; every
/// other C0 control character (0x00-0x1F) and DEL (0x7F) is replaced with
/// the Unicode replacement character so the presence of hidden bytes is
/// still visible rather than silently dropped.
fn sanitize(s: &str) -> String {
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
    pub usage: polaris_provider::Usage,
}

pub fn render_chat(
    frame: &mut Frame,
    session: &Session,
    input: &str,
    status: &Status,
    header: &HeaderInfo,
) {
    let area = frame.area();
    let [header_area, history_area, status_area, input_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(area);

    let header_text = format!(
        "{} / {} — tokens: in {} / out {} / total {}",
        sanitize(header.provider_name),
        sanitize(header.model_name),
        header.usage.input_tokens,
        header.usage.output_tokens,
        header.usage.total_tokens,
    );
    frame.render_widget(Paragraph::new(header_text), header_area);

    let lines = history_lines(session);
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

    let status_text = match status {
        Status::Idle => String::new(),
        Status::Thinking => "thinking...".to_string(),
        Status::Error(e) => format!("error: {e}"),
    };
    frame.render_widget(Paragraph::new(sanitize(&status_text)), status_area);

    frame.render_widget(
        Paragraph::new(sanitize(input))
            .block(Block::default().borders(Borders::ALL).title("input")),
        input_area,
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Thinking, &header))
            .expect("draw");

        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("thinking"));
    }

    #[test]
    fn history_longer_than_the_screen_scrolls_to_show_the_newest_message() {
        let mut session = Session::default();
        for i in 0..30 {
            session.push_user(&format!("message number {i}"));
        }

        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
    fn the_approval_modal_shows_the_reason() {
        let backend = TestBackend::new(60, 10);
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
        let backend = TestBackend::new(60, 10);
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &status, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: 140,
            },
        };

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
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
