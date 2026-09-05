//! Session persistence: one JSON `Message` per line.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

    Ok((
        Session {
            messages,
            ..Session::default()
        },
        truncated,
    ))
}

/// Appends one message as a single JSON line. Creates the file if it
/// doesn't exist yet.
pub fn append_message(path: &Path, message: &Message) -> io::Result<()> {
    let mut options = private_options();
    let mut file = options.create(true).append(true).open(path)?;
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
    atomic_write(path, true, |file| {
        for m in messages {
            serde_json::to_writer(&mut *file, m)?;
            file.write_all(b"\n")?;
        }
        Ok(())
    })
}

/// A complete pre-compaction snapshot. This is historical data, never
/// instructions or approval to execute actions. `project_id` is the canonical
/// project directory, and `session_id` is the resumable log's file stem.
#[derive(Debug, Serialize, Deserialize)]
pub struct ArchiveSnapshot {
    pub version: u32,
    pub project_id: String,
    pub session_id: String,
    pub archived_at_millis: u128,
    pub messages: Vec<Message>,
}

/// Resolves aliases (including symlinks) without conflating directories that
/// happen to have the same basename. Missing projects return an error rather
/// than silently acquiring a different identity. Performs no writes.
pub fn project_identity(project: &Path) -> io::Result<String> {
    let canonical = project.canonicalize()?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "project is not a directory",
        ));
    }
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "project path is not UTF-8"))
}

/// Archive snapshots live outside the resumable JSONL namespace.
pub fn archive_dir(session_path: &Path) -> PathBuf {
    session_path.with_extension("archive")
}

/// Saves all original Message fields before a caller compacts or replaces
/// history. An error must abort that destructive operation. Never changes the
/// resumable log, and never overwrites a previous snapshot. No automatic import
/// of existing session logs is performed. On Unix directories are 0700 and
/// files 0600; on other platforms access follows the user's directory ACLs.
pub fn archive_snapshot(
    session_path: &Path,
    project: &Path,
    messages: &[Message],
) -> io::Result<PathBuf> {
    let project_id = project_identity(project)?;
    let session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid session path"))?;
    let archived_at_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let snapshot = ArchiveSnapshot {
        version: 1,
        project_id,
        session_id: session_id.to_owned(),
        archived_at_millis,
        messages: messages.to_vec(),
    };
    let dir = archive_dir(session_path);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&dir) {
        Ok(()) => sync_parent(&dir)?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // Do not follow an archive-directory symlink into another project's data.
    if !std::fs::symlink_metadata(&dir)?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "archive is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    loop {
        let path = dir.join(format!(
            "{archived_at_millis}-{}-{}.json",
            std::process::id(),
            next_id()
        ));
        match atomic_write(&path, false, |file| {
            serde_json::to_writer(file, &snapshot).map_err(io::Error::from)
        }) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Strict, read-only access for retrieval. Unlike `load_session`, malformed or
/// truncated data returns an error and is never repaired, shortened or created.
pub fn read_archive(path: &Path) -> io::Result<ArchiveSnapshot> {
    let snapshot: ArchiveSnapshot = serde_json::from_slice(&std::fs::read(path)?)?;
    if snapshot.version != 1
        || snapshot.session_id.is_empty()
        || !Path::new(&snapshot.project_id).is_absolute()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid archive provenance or version",
        ));
    }
    Ok(snapshot)
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

struct PendingFile {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        // Close first so cleanup also works on Windows.
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(parent_dir(path))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Writes and syncs a private sibling before publication. Every failure before
/// publication leaves the old file intact. A directory-sync error after
/// publication can report an error with the complete new file already visible.
fn atomic_write(
    path: &Path,
    replace: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let mut pending = loop {
        let candidate =
            parent_dir(path).join(format!(".polaris-{}-{}.tmp", std::process::id(), next_id()));
        match private_options().create_new(true).open(&candidate) {
            Ok(file) => {
                break PendingFile {
                    path: candidate,
                    file: Some(file),
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let file = pending.file.as_mut().expect("pending file is open");
    write(file)?;
    file.sync_all()?;
    pending.file.take();
    if replace {
        std::fs::rename(&pending.path, path)?;
    } else {
        // Atomic create-if-absent: never clobber an earlier archive, even if
        // separate processes happen to choose the same snapshot name.
        std::fs::hard_link(&pending.path, path)?;
    }
    drop(pending);
    sync_parent(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_replaces_a_long_log_and_clear_leaves_no_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append_message(&path, &Message::user("old".repeat(1024))).unwrap();
        rewrite(&path, &[Message::assistant("short")]).unwrap();
        let (session, truncated) = load_session(&path).unwrap();
        assert!(!truncated);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "short");
        clear_session(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn partial_write_failure_preserves_original_bytes_and_removes_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // Include a damaged tail: even recovery must not destroy it until the
        // replacement has been completely written and synced.
        let original = b"{\"role\":\"user\",\"content\":\"keep\"}\n{broken";
        std::fs::write(&path, original).unwrap();
        let result = atomic_write(&path, true, |file| {
            file.write_all(b"partial replacement")?;
            assert_eq!(std::fs::read(&path)?, original);
            Err(io::Error::other("injected write failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn publication_failure_cleans_up_and_preserves_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("sentinel"), b"keep").unwrap();
        assert!(rewrite(&path, &[Message::user("new")]).is_err());
        assert_eq!(std::fs::read(path.join("sentinel")).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_and_recovery_publish_new_inodes_without_truncating_open_readers() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let original = b"{\"role\":\"user\",\"content\":\"keep\"}\n{broken";
        std::fs::write(&path, original).unwrap();
        let mut old_reader = File::open(&path).unwrap();
        let (session, truncated) = load_session(&path).unwrap();
        assert!(truncated);
        assert_eq!(session.messages[0].content, "keep");
        let mut old_bytes = Vec::new();
        old_reader.read_to_end(&mut old_bytes).unwrap();
        assert_eq!(old_bytes, original);
        assert!(!load_session(&path).unwrap().1);
    }

    #[test]
    fn archive_preserves_full_messages_and_provenance_separately_from_resume() {
        use polaris_provider::{ReasoningItem, ToolCall};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-42.jsonl");
        let messages = vec![
            Message::user("元の記録"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                }],
            )
            .with_reasoning(vec![ReasoningItem {
                id: "reasoning-1".into(),
                encrypted_content: "opaque-original".into(),
            }]),
            Message::tool_result("call-1", "full result"),
        ];
        rewrite(&path, &messages).unwrap();
        let before = std::fs::read(&path).unwrap();
        let archived_path = archive_snapshot(&path, dir.path(), &messages).unwrap();
        assert_eq!(archived_path.parent().unwrap(), archive_dir(&path));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let archive = read_archive(&archived_path).unwrap();
        assert_eq!(archive.version, 1);
        assert_eq!(archive.session_id, "session-42");
        assert_eq!(archive.project_id, project_identity(dir.path()).unwrap());
        assert!(archive.archived_at_millis > 0);
        assert_eq!(
            serde_json::to_value(&archive.messages).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
        rewrite(&path, &[Message::user("summary")]).unwrap();
        assert_eq!(read_archive(&archived_path).unwrap().messages.len(), 3);
        assert_eq!(
            load_session(&path).unwrap().0.messages[0].content,
            "summary"
        );
    }

    #[test]
    fn subsequent_snapshots_never_overwrite_earlier_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let first = archive_snapshot(&path, dir.path(), &[Message::user("first")]).unwrap();
        let second = archive_snapshot(&path, dir.path(), &[Message::user("second")]).unwrap();
        assert_ne!(first, second);
        assert_eq!(read_archive(&first).unwrap().messages[0].content, "first");
        assert_eq!(read_archive(&second).unwrap().messages[0].content, "second");
        assert_eq!(std::fs::read_dir(archive_dir(&path)).unwrap().count(), 2);
        assert!(!path.exists());
        // Even an explicitly colliding destination is create-only.
        let bytes = std::fs::read(&first).unwrap();
        assert_eq!(
            atomic_write(&first, false, |f| f.write_all(b"replacement"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&first).unwrap(), bytes);
    }

    #[test]
    fn archive_errors_leave_resume_and_existing_archives_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        append_message(&path, &Message::user("keep")).unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::write(archive_dir(&path), b"not a directory").unwrap();
        assert!(archive_snapshot(&path, dir.path(), &[Message::user("new")]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read(archive_dir(&path)).unwrap(),
            b"not a directory"
        );
    }

    #[test]
    fn archive_read_is_strict_and_never_repairs_or_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.json");
        assert_eq!(
            read_archive(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!path.exists());
        let bytes = b"{\"version\":1,\"messages\":[";
        std::fs::write(&path, bytes).unwrap();
        assert!(read_archive(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let valid = archive_snapshot(&dir.path().join("s.jsonl"), dir.path(), &[]).unwrap();
        let before = std::fs::read(&valid).unwrap();
        assert!(read_archive(&valid).unwrap().messages.is_empty());
        assert_eq!(std::fs::read(&valid).unwrap(), before);
        let mut unsupported: serde_json::Value = serde_json::from_slice(&before).unwrap();
        unsupported["version"] = serde_json::json!(99);
        std::fs::write(&valid, serde_json::to_vec(&unsupported).unwrap()).unwrap();
        assert_eq!(
            read_archive(&valid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn project_identity_separates_same_named_projects_and_rejects_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a/project");
        let second = dir.path().join("b/project");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        assert_ne!(
            project_identity(&first).unwrap(),
            project_identity(&second).unwrap()
        );
        assert_eq!(
            project_identity(&first).unwrap(),
            project_identity(&first.join(".")).unwrap()
        );
        assert!(project_identity(&dir.path().join("missing")).is_err());
        let session = dir.path().join("s.jsonl");
        for project in [&first, &second] {
            let snapshot = archive_snapshot(&session, project, &[]).unwrap();
            assert_eq!(
                read_archive(&snapshot).unwrap().project_id,
                project_identity(project).unwrap()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn archive_permissions_are_private_and_symlink_aliases_preserve_project_identity() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let alias = dir.path().join("alias");
        symlink(&project, &alias).unwrap();
        assert_eq!(
            project_identity(&project).unwrap(),
            project_identity(&alias).unwrap()
        );
        let session = dir.path().join("s.jsonl");
        let path = archive_snapshot(&session, &alias, &[Message::user("private")]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(archive_dir(&session))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        rewrite(&session, &[Message::user("private")]).unwrap();
        assert_eq!(
            std::fs::metadata(&session).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let linked_session = dir.path().join("linked.jsonl");
        symlink(archive_dir(&session), archive_dir(&linked_session)).unwrap();
        assert!(archive_snapshot(&linked_session, &project, &[]).is_err());
        assert_eq!(std::fs::read_dir(archive_dir(&session)).unwrap().count(), 1);
    }

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
