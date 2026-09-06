//! Copies text to the system clipboard via the OSC 52 terminal escape
//! sequence — the mechanism codex's own `/copy`/`Ctrl+O` fall back to for
//! SSH/tmux sessions (see its `clipboard_copy.rs`). polaris uses OSC 52
//! unconditionally rather than also trying a native clipboard crate first:
//! it's supported by the terminals this project already tests against
//! (iTerm2, WezTerm, kitty, Ghostty — with iTerm2 needing "Applications in
//! terminal may access clipboard" enabled) and needs no new heavy
//! dependency. A terminal with no OSC 52 support just silently ignores the
//! sequence — there is no reliable way to detect that from the app side.

use std::io::Write;

/// Maximum raw bytes base64-encoded into an OSC 52 sequence — matches
/// codex's own limit, well past anything a single reply's text will hit
/// while still bounding worst-case escape-sequence size.
const OSC52_MAX_RAW_BYTES: usize = 100_000;

/// Builds the OSC 52 escape sequence that sets the terminal clipboard to
/// `text`. Wrapped in a tmux DCS passthrough when running inside tmux,
/// since tmux otherwise swallows the raw sequence instead of forwarding it
/// to the outer terminal. Errors (instead of silently truncating) when
/// `text` exceeds `OSC52_MAX_RAW_BYTES`.
fn osc52_sequence(text: &str, tmux: bool) -> Result<String, String> {
    use base64::Engine;
    let raw_bytes = text.len();
    if raw_bytes > OSC52_MAX_RAW_BYTES {
        return Err(format!(
            "text too large to copy ({raw_bytes} bytes; max {OSC52_MAX_RAW_BYTES})"
        ));
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if tmux {
        Ok(format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\"))
    } else {
        Ok(format!("\x1b]52;c;{encoded}\x07"))
    }
}

fn write_osc52_to_writer(mut writer: impl Write, sequence: &str) -> Result<(), String> {
    writer
        .write_all(sequence.as_bytes())
        .map_err(|e| format!("failed to write OSC 52: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("failed to flush OSC 52: {e}"))
}

/// Copies `text` to the system clipboard by writing an OSC 52 sequence to
/// `/dev/tty` — not stdout, since stdout is what `ratatui` is mid-frame
/// drawing into, and interleaving an escape sequence there risks corrupting
/// the next draw. Falls back to stdout only if `/dev/tty` can't be opened
/// (e.g. it's been redirected away in some unusual invocation).
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let tmux = std::env::var_os("TMUX").is_some();
    let sequence = osc52_sequence(text, tmux)?;
    #[cfg(unix)]
    {
        if let Ok(tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            return write_osc52_to_writer(tty, &sequence);
        }
    }
    write_osc52_to_writer(std::io::stdout().lock(), &sequence)
}

/// The text of the most recent assistant reply, or `None` when there isn't
/// one yet (a fresh session, or one that's only seen user/tool messages so
/// far — e.g. a turn still in flight). Skips assistant messages with empty
/// `content` (a tool-call-only turn carries no reply text of its own).
pub fn last_agent_message_text(session: &polaris_core::session::Session) -> Option<&str> {
    session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == polaris_provider::Role::Assistant && !m.content.is_empty())
        .map(|m| m.content.as_str())
}

/// The `/copy` command's core logic, with the actual clipboard write
/// injected so tests can exercise the "nothing to copy" / "copied" /
/// "can't copy" status text without ever touching a real tty. Production
/// callers pass `copy_to_clipboard` itself.
pub fn copy_last_reply_with(
    session: &polaris_core::session::Session,
    copy_fn: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    copy_text_with(last_agent_message_text(session), copy_fn)
}

/// Same status-text logic as `copy_last_reply_with`, but taking the text
/// directly instead of a `Session` — for callers that already have (or can
/// only have) a snapshot of the reply text rather than the live session.
/// `run()`'s mid-turn key handler is one: `session` is mutably borrowed by
/// the in-flight turn there, so it captures the pre-turn reply text once,
/// up front, and passes it here on every `Ctrl+O` instead of re-reading
/// `session`.
pub fn copy_text_with(
    text: Option<&str>,
    copy_fn: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    match text {
        None => "nothing to copy yet".to_string(),
        Some(text) => match copy_fn(text) {
            Ok(()) => "copied last reply to the clipboard".to_string(),
            Err(e) => format!("can't copy: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::session::Session;

    #[test]
    fn osc52_sequence_encodes_the_text_as_base64() {
        use base64::Engine;
        let sequence = osc52_sequence("hello", false).expect("sequence");
        let encoded = sequence
            .trim_start_matches("\u{1b}]52;c;")
            .trim_end_matches('\u{7}');
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn osc52_sequence_rejects_a_payload_past_the_limit() {
        let text = "x".repeat(OSC52_MAX_RAW_BYTES + 1);
        assert!(osc52_sequence(&text, false).is_err());
    }

    #[test]
    fn osc52_sequence_accepts_a_payload_exactly_at_the_limit() {
        // Pins the boundary itself, not just one byte past it — a future
        // `>` -> `>=` typo in the guard would still fail
        // `osc52_sequence_rejects_a_payload_past_the_limit` but this test
        // is what actually catches it.
        let text = "x".repeat(OSC52_MAX_RAW_BYTES);
        assert!(osc52_sequence(&text, false).is_ok());
    }

    #[test]
    fn osc52_sequence_wraps_tmux_passthrough() {
        assert_eq!(
            osc52_sequence("hi", true),
            Ok("\u{1b}Ptmux;\u{1b}\u{1b}]52;c;aGk=\u{7}\u{1b}\\".to_string())
        );
    }

    #[test]
    fn write_osc52_to_writer_emits_the_sequence_verbatim() {
        let sequence = "\u{1b}]52;c;aGVsbG8=\u{7}";
        let mut output = Vec::new();
        assert_eq!(write_osc52_to_writer(&mut output, sequence), Ok(()));
        assert_eq!(output, sequence.as_bytes());
    }

    #[test]
    fn last_agent_message_text_returns_the_most_recent_assistant_reply() {
        let mut session = Session::new();
        session.push_user("hi");
        session.push_assistant("first reply", Vec::new());
        session.push_user("again");
        session.push_assistant("second reply", Vec::new());
        assert_eq!(last_agent_message_text(&session), Some("second reply"));
    }

    #[test]
    fn last_agent_message_text_skips_a_trailing_tool_only_assistant_turn() {
        let mut session = Session::new();
        session.push_user("hi");
        session.push_assistant("real reply", Vec::new());
        session.push_assistant_tool_calls("", Vec::new(), Vec::new());
        assert_eq!(last_agent_message_text(&session), Some("real reply"));
    }

    #[test]
    fn last_agent_message_text_is_none_before_any_reply() {
        let mut session = Session::new();
        session.push_user("hi");
        assert_eq!(last_agent_message_text(&session), None);
    }

    #[test]
    fn copy_last_reply_with_nothing_to_copy_never_calls_the_copy_fn() {
        let mut session = Session::new();
        session.push_user("hi");
        let status = copy_last_reply_with(&session, |_| panic!("should not be called"));
        assert!(status.contains("nothing to copy"));
    }

    #[test]
    fn copy_last_reply_with_a_reply_reports_success() {
        let mut session = Session::new();
        session.push_assistant("the answer is 4", Vec::new());
        let status = copy_last_reply_with(&session, |text| {
            assert_eq!(text, "the answer is 4");
            Ok(())
        });
        assert!(status.contains("copied"));
    }

    #[test]
    fn copy_last_reply_with_a_failing_copy_reports_the_error() {
        let mut session = Session::new();
        session.push_assistant("reply", Vec::new());
        let status = copy_last_reply_with(&session, |_| Err("no tty".to_string()));
        assert!(status.contains("can't copy"));
        assert!(status.contains("no tty"));
    }

    #[test]
    fn copy_text_with_none_never_calls_the_copy_fn() {
        let status = copy_text_with(None, |_| panic!("should not be called"));
        assert!(status.contains("nothing to copy"));
    }

    #[test]
    fn copy_text_with_some_text_copies_it_verbatim() {
        let status = copy_text_with(Some("snapshot text"), |text| {
            assert_eq!(text, "snapshot text");
            Ok(())
        });
        assert!(status.contains("copied"));
    }
}
