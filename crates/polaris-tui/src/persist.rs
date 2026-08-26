//! Session persistence: one JSON `Message` per line.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Splits raw file bytes into lines, keeping the newline convention of
/// `BufRead::lines()` (split on `\n`, trailing `\r` trimmed) but without
/// its UTF-8-or-bust behavior: a line with invalid UTF-8 bytes is decoded
/// lossily rather than propagating an `InvalidData` error out of the
/// caller.
fn read_lines_lossy(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == b'\n')
        .map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            String::from_utf8_lossy(line).into_owned()
        })
        .collect()
}

use polaris_core::session::Session;
use polaris_provider::Message;
use serde::{Deserialize, Serialize};

/// Loads a session from `path`. Returns `(session, true)` when a corrupt
/// line was found; everything from that line onward is dropped both from
/// the returned `Session` and from the file on disk, so a later reload
/// (or a later `append_message`) never re-encounters it. A missing file
/// loads as an empty session, not an error — there is nothing to resume
/// yet on first run.
pub fn load_session(path: &Path) -> io::Result<(Session, bool)> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Session::default(), false)),
        Err(e) => return Err(e),
    };

    let mut messages = Vec::new();
    let mut truncated = false;
    // Decoded lossily rather than via `BufRead::lines()`, which errors out
    // of this function entirely on the first invalid-UTF-8 byte instead of
    // going through the same corrupt-line recovery as bad JSON.
    for line in read_lines_lossy(&bytes) {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(&line) {
            Ok(m) => messages.push(m),
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }

    if truncated {
        rewrite(path, &messages)?;
    }

    Ok((Session { messages }, truncated))
}

/// Appends one message as a single JSON line. Creates the file if it
/// doesn't exist yet.
pub fn append_message(path: &Path, message: &Message) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(message).expect("Message always serializes");
    writeln!(file, "{line}")
}

/// Empties the persisted session file. Used by the `/clear` slash command
/// — the in-memory `Session` is cleared by the caller; this keeps the file
/// on disk from resurrecting the old conversation on the next launch.
pub fn clear_session(path: &Path) -> io::Result<()> {
    rewrite(path, &[])
}

/// Recorded once per saved conversation, alongside its `<id>.jsonl`
/// message log — the directory it was started from and when, so
/// `/resume` can group and sort conversations without re-deriving that
/// from the messages themselves.
#[derive(Serialize, Deserialize)]
pub struct SessionMeta {
    pub cwd: String,
    pub started_at_millis: u128,
}

/// Writes `<id>.meta.json` next to a session's message log, but only if
/// it doesn't exist yet. Session creation is lazy — nothing touches disk
/// until the first message is actually sent — so this is called before
/// every append, not just the first one; the existence check makes every
/// call after the first a no-op instead of re-stamping `started_at`.
pub fn write_meta_if_absent(meta_path: &Path, meta: &SessionMeta) -> io::Result<()> {
    if meta_path.exists() {
        return Ok(());
    }
    let json = serde_json::to_string(meta).expect("SessionMeta always serializes");
    std::fs::write(meta_path, json)
}

/// Reads back a session's metadata. `None` (not an error) when the file
/// is missing or unparseable — a `/resume` listing skips a corrupt or
/// incomplete entry rather than failing the whole list.
pub fn read_meta(meta_path: &Path) -> Option<SessionMeta> {
    let bytes = std::fs::read(meta_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn rewrite(path: &Path, messages: &[Message]) -> io::Result<()> {
    let mut file = File::create(path)?;
    for m in messages {
        let line = serde_json::to_string(m).expect("Message always serializes");
        writeln!(file, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_loads_as_an_empty_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        let (session, truncated) = load_session(&path).expect("load");

        assert!(session.messages.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn appended_messages_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        append_message(&path, &Message::user("hi")).expect("append");
        append_message(&path, &Message::assistant("hello")).expect("append");

        let (session, truncated) = load_session(&path).expect("load");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "hi");
        assert_eq!(session.messages[1].content, "hello");
        assert!(!truncated);
    }

    #[test]
    fn an_invalid_utf8_line_is_handled_like_a_corrupt_json_line_not_a_hard_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        append_message(&path, &Message::user("good line")).expect("append");
        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        // Invalid UTF-8 bytes (a lone continuation byte), not valid JSON either way.
        file.write_all(b"\xff\xfe not valid utf-8\n")
            .expect("write invalid utf-8 line");
        drop(file);
        append_message(&path, &Message::user("orphaned, after the invalid line")).expect("append");

        let (session, truncated) = load_session(&path).expect("load");

        assert!(truncated);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "good line");

        // Reloading again must be clean now that the invalid tail was rewritten away.
        let (reloaded, truncated_again) = load_session(&path).expect("reload");
        assert_eq!(reloaded.messages.len(), 1);
        assert!(!truncated_again);
    }

    #[test]
    fn a_corrupt_line_truncates_and_rewrites_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tui-session.jsonl");

        append_message(&path, &Message::user("good line")).expect("append");
        let mut file = OpenOptions::new().append(true).open(&path).expect("open");
        writeln!(file, "{{not valid json").expect("write corrupt line");
        drop(file);
        append_message(&path, &Message::user("orphaned, after the corrupt line")).expect("append");

        let (session, truncated) = load_session(&path).expect("load");

        assert!(truncated);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "good line");

        // Reloading again must be clean now that the corrupt tail was rewritten away.
        let (reloaded, truncated_again) = load_session(&path).expect("reload");
        assert_eq!(reloaded.messages.len(), 1);
        assert!(!truncated_again);
    }

    #[test]
    fn meta_written_once_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("abc.meta.json");

        write_meta_if_absent(
            &path,
            &SessionMeta {
                cwd: "/tmp/example".to_string(),
                started_at_millis: 1_705_311_000_000,
            },
        )
        .expect("write meta");

        let meta = read_meta(&path).expect("meta should parse");
        assert_eq!(meta.cwd, "/tmp/example");
        assert_eq!(meta.started_at_millis, 1_705_311_000_000);
    }

    #[test]
    fn a_second_write_meta_if_absent_call_does_not_overwrite_the_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("abc.meta.json");

        write_meta_if_absent(
            &path,
            &SessionMeta {
                cwd: "/tmp/first".to_string(),
                started_at_millis: 100,
            },
        )
        .expect("first write");
        write_meta_if_absent(
            &path,
            &SessionMeta {
                cwd: "/tmp/second".to_string(),
                started_at_millis: 200,
            },
        )
        .expect("second write is a no-op");

        let meta = read_meta(&path).expect("meta should parse");
        assert_eq!(meta.cwd, "/tmp/first");
        assert_eq!(meta.started_at_millis, 100);
    }

    #[test]
    fn read_meta_on_a_missing_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("missing.meta.json");
        assert!(read_meta(&path).is_none());
    }

    #[test]
    fn read_meta_on_a_corrupt_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("corrupt.meta.json");
        std::fs::write(&path, b"{not valid json").expect("write corrupt meta");
        assert!(read_meta(&path).is_none());
    }
}
