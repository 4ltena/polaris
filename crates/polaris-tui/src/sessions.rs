//! Enumerates saved conversations under `~/.polaris/sessions/` for the
//! `/resume` picker. One `<id>.jsonl` + `<id>.meta.json` pair per
//! conversation (see `persist.rs`). A conversation with zero messages —
//! created but never actually used — is skipped, since there's nothing
//! to resume.

use std::path::{Path, PathBuf};

use polaris_core::session_store::PersistedSession;

use crate::persist;

const PREVIEW_CHARS: usize = 60;

/// Shared bounds for the resume picker and exact saved-selection checks.
#[derive(Clone, Copy, Debug)]
pub struct SessionListLimits {
    pub max_entries: usize,
    pub max_sessions: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    pub max_messages: usize,
}
impl Default for SessionListLimits {
    fn default() -> Self {
        Self {
            max_entries: 4096,
            max_sessions: 256,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            max_messages: 20_000,
        }
    }
}

fn list_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(target_os = "linux")]
fn directory_entries(path: &Path, max: usize) -> std::io::Result<Vec<PathBuf>> {
    polaris_core::project_mutation::open_directory_nofollow(path)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        if entries.len() >= max {
            return Err(list_error("Saved session entry count exceeds the limit"));
        }
        entries.push(entry?.path());
    }
    entries.sort();
    Ok(entries)
}
#[cfg(not(target_os = "linux"))]
fn directory_entries(_path: &Path, _max: usize) -> std::io::Result<Vec<PathBuf>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Boundary-verified session listing is unavailable on this OS",
    ))
}

/// A descriptor-bound read; never repairs a legacy tail or initializes SQLite.
#[cfg(target_os = "linux")]
pub(crate) fn read_bounded_session_file(
    path: &Path,
    max: usize,
    total: &mut usize,
    total_max: usize,
) -> std::io::Result<Vec<u8>> {
    use std::{
        ffi::CString,
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };
    let max = max.min(total_max.saturating_sub(*total));
    let parent = polaris_core::project_mutation::open_directory_nofollow(
        path.parent()
            .ok_or_else(|| list_error("The storage file has no parent"))?,
    )?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| list_error("Invalid saved filename"))?
            .as_bytes(),
    )
    .map_err(std::io::Error::other)?;
    let protection = polaris_auth::protection::with_protected_paths(Clone::clone)
        .map_err(|_| list_error("Cannot verify the protected file inventory"))?;
    if protection.paths().contains(path) {
        return Err(list_error("Protected files cannot be read as sessions"));
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() > max as u64
        || protection
            .identities()
            .contains(&polaris_auth::protection::ProtectedIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
    {
        return Err(list_error(
            "The saved session is protected or exceeds the size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(list_error("Saved session read size exceeds the limit"));
    }
    *total = total
        .checked_add(bytes.len())
        .ok_or_else(|| list_error("Saved session total size exceeds the limit"))?;
    if *total > total_max {
        return Err(list_error("Saved session total size exceeds the limit"));
    }
    Ok(bytes)
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn read_bounded_session_file(
    _path: &Path,
    _max: usize,
    _total: &mut usize,
    _total_max: usize,
) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Boundary-verified session reading is unavailable on this OS",
    ))
}

pub(crate) fn parse_legacy_messages(
    bytes: &[u8],
    max_messages: usize,
) -> std::io::Result<Vec<polaris_provider::Message>> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| list_error("The saved session is not UTF-8"))?;
    let mut messages = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if messages.len() >= max_messages {
            return Err(list_error("Saved session message count exceeds the limit"));
        }
        messages.push(
            serde_json::from_str(line).map_err(|_| {
                list_error("Invalid saved session JSONL. Original text is unchanged")
            })?,
        );
    }
    Ok(messages)
}

/// Missing namespaces are empty; unreadable, corrupt or oversized evidence is
/// an explicit error. The old Vec-returning API remains for existing callers.
pub fn try_list_sessions(sessions_dir: &Path) -> std::io::Result<Vec<SessionSummary>> {
    try_list_sessions_bounded(sessions_dir, SessionListLimits::default())
}
pub fn try_list_sessions_bounded(
    sessions_dir: &Path,
    limits: SessionListLimits,
) -> std::io::Result<Vec<SessionSummary>> {
    let entries = match directory_entries(sessions_dir, limits.max_entries) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut total = 0;
    let mut out = Vec::new();
    for path in entries {
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".meta.json"))
        else {
            continue;
        };
        polaris_memory::management::validate_session_id(id).map_err(std::io::Error::other)?;
        let meta_bytes = read_bounded_session_file(
            &path,
            limits.max_file_bytes.min(64 * 1024),
            &mut total,
            limits.max_total_bytes,
        )?;
        let meta: persist::SessionMeta = serde_json::from_slice(&meta_bytes)
            .map_err(|_| list_error("Invalid saved session metadata"))?;
        if meta.cwd.is_empty() || meta.cwd.len() > 16_384 {
            return Err(list_error("Invalid saved session project name"));
        }
        let session_path = sessions_dir.join(format!("{id}.jsonl"));
        let bytes = match read_bounded_session_file(
            &session_path,
            limits.max_file_bytes,
            &mut total,
            limits.max_total_bytes,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let messages = parse_legacy_messages(&bytes, limits.max_messages)?;
        if messages.is_empty() {
            continue;
        }
        if out.len() >= limits.max_sessions {
            return Err(list_error(
                "Displayable saved session count exceeds the limit",
            ));
        }
        let preview = messages
            .iter()
            .find(|m| m.role == polaris_provider::Role::User)
            .map(|m| preview_of(&m.content))
            .unwrap_or_default();
        out.push(SessionSummary {
            id: id.into(),
            path: session_path,
            cwd: meta.cwd,
            started_at_millis: meta.started_at_millis,
            message_count: messages.len(),
            preview,
            kind: SessionKind::Legacy,
        });
    }
    Ok(out)
}

pub fn try_list_v2_sessions(
    data_root: &Path,
    database: &Path,
    project_id: &str,
    cwd: &str,
) -> std::io::Result<Vec<SessionSummary>> {
    try_list_v2_sessions_filtered(data_root, database, project_id, cwd, |_, _| Ok(true))
}

/// The filter sees already bounded saved state, avoiding PersistedSession's
/// SQLite initialization on this read-only list path. Errors stay explicit.
pub fn try_list_v2_sessions_filtered(
    data_root: &Path,
    _database: &Path,
    project_id: &str,
    cwd: &str,
    include: impl FnMut(
        &SessionSummary,
        &polaris_core::conversation_state::ConversationStateV2,
    ) -> std::io::Result<bool>,
) -> std::io::Result<Vec<SessionSummary>> {
    try_list_v2_sessions_filtered_bounded(
        data_root,
        project_id,
        cwd,
        SessionListLimits::default(),
        include,
    )
}
/// Hides unpublished rewind children using the full shared provenance guard.
#[cfg(unix)]
pub fn try_list_published_v2_sessions(
    owner: &polaris_core::rewind_workspace::WorkspaceOwner,
    data_root: &Path,
    database: &Path,
    project_id: &str,
    cwd: &str,
) -> std::io::Result<Vec<SessionSummary>> {
    let mut reads = polaris_core::rewind_admission::PublicationReadBudgetV1::default();
    try_list_v2_sessions_filtered(data_root, database, project_id, cwd, |summary, _| {
        match polaris_core::rewind_admission::published_session_bounded(
            owner,
            data_root,
            project_id,
            &summary.id,
            &mut reads,
        ) {
            Ok(_) => Ok(true),
            Err(error) if reads.evidence_read_failed() => Err(error),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                Err(error)
            }
            Err(_) => Ok(false),
        }
    })
}
pub fn try_list_v2_sessions_filtered_bounded(
    data_root: &Path,
    project_id: &str,
    cwd: &str,
    limits: SessionListLimits,
    mut include: impl FnMut(
        &SessionSummary,
        &polaris_core::conversation_state::ConversationStateV2,
    ) -> std::io::Result<bool>,
) -> std::io::Result<Vec<SessionSummary>> {
    use polaris_core::conversation_state::{ConversationStateV2, RawEventV2, content_hash};
    let directory = data_root.join("sessions-v2");
    let entries = match directory_entries(&directory, limits.max_entries) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut total = 0;
    let mut out = Vec::new();
    for path in entries {
        let id = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| list_error("Saved session ID is not UTF-8"))?;
        if !std::fs::symlink_metadata(&path)?.is_dir() {
            return Err(list_error(
                "The saved v2 session is not a regular directory",
            ));
        }
        let state_bytes = read_bounded_session_file(
            &path.join("state.json"),
            limits.max_file_bytes,
            &mut total,
            limits.max_total_bytes,
        )?;
        let state: ConversationStateV2 = serde_json::from_slice(&state_bytes)
            .map_err(|_| list_error("Invalid v2 session storage state"))?;
        state.validate()?;
        if state.session_id != id {
            return Err(list_error("V2 session ID and storage location mismatch"));
        }
        if state.project_id != project_id {
            continue;
        }
        let raw = read_bounded_session_file(
            &path.join("raw.jsonl"),
            limits.max_file_bytes,
            &mut total,
            limits.max_total_bytes,
        )?;
        let committed = usize::try_from(state.raw_commit_offset)
            .map_err(|_| list_error("Invalid v2 source storage range"))?;
        if committed > raw.len()
            || content_hash(&raw[..committed]) != state.raw_commit_hash
            || (!raw.is_empty() && !raw.ends_with(b"\n"))
        {
            return Err(list_error("V2 source length or hash mismatch"));
        }
        let mut count = 0;
        let mut preview = String::new();
        for line in raw.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            if count >= limits.max_messages {
                return Err(list_error("V2 source message count exceeds the limit"));
            }
            let event: RawEventV2 =
                serde_json::from_slice(line).map_err(|_| list_error("Invalid v2 source event"))?;
            if event.sequence != count as u64 + 1 || event.epoch > state.epoch {
                return Err(list_error("Invalid v2 source order or epoch"));
            }
            if preview.is_empty() && event.message.role == polaris_provider::Role::User {
                preview = preview_of(&event.message.content);
            }
            count += 1;
        }
        if count == 0 {
            continue;
        }
        let started_at_millis = std::fs::symlink_metadata(&path)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_millis();
        let summary = SessionSummary {
            id: id.into(),
            path: path.clone(),
            cwd: cwd.into(),
            started_at_millis,
            message_count: count,
            preview,
            kind: SessionKind::V2,
        };
        if !include(&summary, &state)? {
            continue;
        }
        if out.len() >= limits.max_sessions {
            return Err(list_error("Displayable v2 session count exceeds the limit"));
        }
        out.push(summary);
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Legacy,
    V2,
}

#[derive(Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub started_at_millis: u128,
    pub message_count: usize,
    /// The first user message, trimmed to a short one-line preview.
    pub preview: String,
    pub kind: SessionKind,
}

/// Scans `sessions_dir` for every `<id>.meta.json` that has a
/// non-empty `<id>.jsonl` next to it. Order is unspecified — callers
/// sort/group via `grouped`.
pub fn list_sessions(sessions_dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path();
        let Some(id) = meta_path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".meta.json"))
        else {
            continue;
        };
        let Some(meta) = persist::read_meta(&meta_path) else {
            continue;
        };
        let session_path = sessions_dir.join(format!("{id}.jsonl"));
        let Ok((session, _truncated)) = persist::load_session(&session_path) else {
            continue;
        };
        if session.messages.is_empty() {
            continue;
        }

        let preview = session
            .messages
            .iter()
            .find(|m| m.role == polaris_provider::Role::User)
            .map(|m| preview_of(&m.content))
            .unwrap_or_default();

        out.push(SessionSummary {
            id: id.to_string(),
            path: session_path,
            cwd: meta.cwd,
            started_at_millis: meta.started_at_millis,
            message_count: session.messages.len(),
            preview,
            kind: SessionKind::Legacy,
        });
    }
    out
}

/// Lists valid v2 sessions for the current project.  V2 intentionally stores a
/// stable project identity rather than a display path, so these sessions stay
/// in the current-project group in the picker.
#[cfg(any(test, not(unix)))]
pub fn list_v2_sessions(
    data_root: &Path,
    database: &Path,
    project_id: &str,
    cwd: &str,
) -> Vec<SessionSummary> {
    list_v2_sessions_filtered(data_root, database, project_id, cwd, |_| true)
}

/// The caller supplies the execution-publication check used by the picker.
/// A failed child is absent from the list while ordinary sessions may still
/// be shown for read-only inspection if their workspace no longer matches.
pub fn list_v2_sessions_filtered(
    data_root: &Path,
    database: &Path,
    project_id: &str,
    cwd: &str,
    mut include: impl FnMut(&PersistedSession) -> bool,
) -> Vec<SessionSummary> {
    let sessions_dir = data_root.join("sessions-v2");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let persisted = PersistedSession::open(data_root, database, project_id, &id).ok()?;
            if !include(&persisted) {
                return None;
            }
            let snapshot = persisted.snapshot().ok()?;
            if snapshot.state.project_id != project_id || snapshot.events.is_empty() {
                return None;
            }
            let preview = snapshot
                .events
                .iter()
                .find(|event| event.message.role == polaris_provider::Role::User)
                .map(|event| preview_of(&event.message.content))
                .unwrap_or_default();
            let started_at_millis = entry
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis();
            Some(SessionSummary {
                id,
                path: entry.path(),
                cwd: cwd.to_string(),
                started_at_millis,
                message_count: snapshot.events.len(),
                preview,
                kind: SessionKind::V2,
            })
        })
        .collect()
}

fn preview_of(content: &str) -> String {
    let one_line = content.replace('\n', " ");
    let trimmed: String = one_line.chars().take(PREVIEW_CHARS).collect();
    if one_line.chars().count() > PREVIEW_CHARS {
        format!("{trimmed}...")
    } else {
        trimmed
    }
}

pub struct DirGroup {
    pub cwd: String,
    pub sessions: Vec<SessionSummary>,
}

/// Groups summaries by their originating directory — the current
/// directory's group first, then every other directory ordered by its
/// most recent conversation. Within a group, newest conversation first.
pub fn grouped(mut summaries: Vec<SessionSummary>, current_cwd: &str) -> Vec<DirGroup> {
    summaries.sort_by_key(|s| std::cmp::Reverse(s.started_at_millis));

    let mut groups: Vec<DirGroup> = Vec::new();
    for s in summaries {
        match groups.iter_mut().find(|g| g.cwd == s.cwd) {
            Some(g) => g.sessions.push(s),
            None => groups.push(DirGroup {
                cwd: s.cwd.clone(),
                sessions: vec![s],
            }),
        }
    }

    // `summaries` was pre-sorted newest-first, so each group's sessions
    // are already newest-first and the groups themselves already sit in
    // "most recently active first" order — the only thing left is
    // pulling the current directory's group to the front.
    if let Some(pos) = groups.iter().position(|g| g.cwd == current_cwd)
        && pos != 0
    {
        let g = groups.remove(pos);
        groups.insert(0, g);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::{self, SessionMeta};
    use polaris_provider::Message;

    #[cfg(target_os = "linux")]
    #[test]
    fn m3c_resume_list_bounded_normal_and_error_preserve_saved_bytes() {
        const CHILD: &str = "POLARIS_M3C_RESUME_LIST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["sessions::tests::m3c_resume_list_bounded_normal_and_error_preserve_saved_bytes","--exact","--nocapture"]).env(CHILD,"1").env("HOME",home.path()).env_remove("POLARIS_DATA_DIR").output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let directory = home.join(".polaris/sessions");
        std::fs::create_dir_all(&directory).unwrap();
        write_session(
            &directory,
            "saved",
            project.to_str().unwrap(),
            1,
            &[Message::user("bounded historical preview")],
        );
        let path = directory.join("saved.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let normal = try_list_sessions(&directory).unwrap();
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].preview, "bounded historical preview");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let limits = SessionListLimits {
            max_entries: 1,
            ..SessionListLimits::default()
        };
        assert!(try_list_sessions_bounded(&directory, limits).is_err());
        let not_directory = home.join("not-directory");
        std::fs::write(&not_directory, b"file").unwrap();
        assert!(try_list_sessions(&not_directory).is_err());
        std::fs::write(&path, b"{broken-json\n").unwrap();
        assert!(try_list_sessions(&directory).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken-json\n");
        let data = home.join(".polaris");
        let database = data.join("memory.sqlite3");
        let v2 = PersistedSession::create(
            &data,
            &database,
            project.to_str().unwrap(),
            polaris_core::conversation_state::HistoryMode::Legacy,
            None,
        )
        .unwrap();
        v2.append(Message::user("v2 bounded preview"), true, None)
            .unwrap();
        let before = std::fs::read(&database).unwrap();
        let listed = try_list_v2_sessions(
            &data,
            &database,
            project.to_str().unwrap(),
            project.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, SessionKind::V2);
        assert_eq!(listed[0].preview, "v2 bounded preview");
        assert_eq!(std::fs::read(&database).unwrap(), before);
        let raw = v2.store.directory().join("raw.jsonl");
        std::fs::write(&raw, b"invalid raw\n").unwrap();
        assert!(
            try_list_v2_sessions(
                &data,
                &database,
                project.to_str().unwrap(),
                project.to_str().unwrap()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&raw).unwrap(), b"invalid raw\n");
    }

    fn write_session(
        dir: &Path,
        id: &str,
        cwd: &str,
        started_at_millis: u128,
        messages: &[Message],
    ) {
        persist::write_meta_if_absent(
            &dir.join(format!("{id}.meta.json")),
            &SessionMeta {
                cwd: cwd.to_string(),
                started_at_millis,
            },
        )
        .expect("write meta");
        let session_path = dir.join(format!("{id}.jsonl"));
        for m in messages {
            persist::append_message(&session_path, m).expect("append");
        }
    }

    #[test]
    fn a_session_with_no_messages_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_session(dir.path(), "empty", "/tmp/a", 100, &[]);

        assert!(list_sessions(dir.path()).is_empty());
    }

    #[test]
    fn a_session_with_a_missing_meta_file_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A `.jsonl` with no matching `.meta.json` — happens if writing
        // the message succeeded but meta-writing failed, or a future
        // format doesn't pair them the same way.
        persist::append_message(&dir.path().join("orphan.jsonl"), &Message::user("hi"))
            .expect("append");

        assert!(list_sessions(dir.path()).is_empty());
    }

    #[test]
    fn a_real_session_is_listed_with_its_metadata() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_session(
            dir.path(),
            "s1",
            "/tmp/project",
            1_705_311_000_000,
            &[
                Message::user("what does this repo do?"),
                Message::assistant("it's a coding agent"),
            ],
        );

        let sessions = list_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
        assert_eq!(sessions[0].cwd, "/tmp/project");
        assert_eq!(sessions[0].started_at_millis, 1_705_311_000_000);
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].preview, "what does this repo do?");
    }

    #[test]
    fn a_v2_session_is_listed_for_its_project() {
        let root = tempfile::tempdir().expect("temp dir");
        let database = root.path().join("memory.sqlite3");
        let persisted = PersistedSession::create(
            root.path(),
            &database,
            "project-id",
            polaris_core::conversation_state::HistoryMode::Legacy,
            None,
        )
        .expect("create v2 session");
        persisted
            .append(Message::user("resume this v2 conversation"), true, None)
            .expect("append message");

        let sessions = list_v2_sessions(root.path(), &database, "project-id", "/tmp/project");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].kind, SessionKind::V2);
        assert_eq!(sessions[0].preview, "resume this v2 conversation");
        assert!(list_v2_sessions(root.path(), &database, "other-project", "/tmp/other").is_empty());
    }

    #[test]
    fn the_preview_is_truncated_for_a_long_first_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let long = "x".repeat(200);
        write_session(
            dir.path(),
            "s1",
            "/tmp/project",
            100,
            &[Message::user(&long)],
        );

        let sessions = list_sessions(dir.path());
        assert!(sessions[0].preview.ends_with("..."));
        assert!(sessions[0].preview.len() < long.len());
    }

    #[test]
    fn grouped_puts_the_current_directory_first_even_if_older() {
        let a = SessionSummary {
            id: "a".into(),
            path: PathBuf::new(),
            cwd: "/tmp/other".into(),
            started_at_millis: 200,
            message_count: 1,
            preview: String::new(),
            kind: SessionKind::Legacy,
        };
        let b = SessionSummary {
            id: "b".into(),
            path: PathBuf::new(),
            cwd: "/tmp/current".into(),
            started_at_millis: 100,
            message_count: 1,
            preview: String::new(),
            kind: SessionKind::Legacy,
        };

        let groups = grouped(vec![a, b], "/tmp/current");
        assert_eq!(groups[0].cwd, "/tmp/current");
        assert_eq!(groups[1].cwd, "/tmp/other");
    }

    #[test]
    fn sessions_within_a_group_are_newest_first() {
        let old = SessionSummary {
            id: "old".into(),
            path: PathBuf::new(),
            cwd: "/tmp/x".into(),
            started_at_millis: 100,
            message_count: 1,
            preview: String::new(),
            kind: SessionKind::Legacy,
        };
        let new = SessionSummary {
            id: "new".into(),
            path: PathBuf::new(),
            cwd: "/tmp/x".into(),
            started_at_millis: 200,
            message_count: 1,
            preview: String::new(),
            kind: SessionKind::Legacy,
        };

        let groups = grouped(vec![old, new], "/tmp/x");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sessions[0].id, "new");
        assert_eq!(groups[0].sessions[1].id, "old");
    }

    #[test]
    fn other_directories_are_ordered_by_their_most_recent_conversation() {
        let stale = SessionSummary {
            id: "stale".into(),
            path: PathBuf::new(),
            cwd: "/tmp/stale-dir".into(),
            started_at_millis: 50,
            message_count: 1,
            preview: String::new(),
            kind: SessionKind::Legacy,
        };
        let fresh = SessionSummary {
            id: "fresh".into(),
            path: PathBuf::new(),
            cwd: "/tmp/fresh-dir".into(),
            started_at_millis: 300,
            message_count: 1,
            preview: String::new(),
            kind: SessionKind::Legacy,
        };

        // Neither is the current directory, so order is purely recency.
        let groups = grouped(vec![stale, fresh], "/tmp/current");
        assert_eq!(groups[0].cwd, "/tmp/fresh-dir");
        assert_eq!(groups[1].cwd, "/tmp/stale-dir");
    }
}
