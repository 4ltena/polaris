//! Enumerates saved conversations under `~/.polaris/sessions/` for the
//! `/resume` picker. One `<id>.jsonl` + `<id>.meta.json` pair per
//! conversation (see `persist.rs`). A conversation with zero messages —
//! created but never actually used — is skipped, since there's nothing
//! to resume.

use std::path::{Path, PathBuf};

use polaris_core::session_store::PersistedSession;

use crate::persist;

const PREVIEW_CHARS: usize = 60;

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
pub fn list_v2_sessions(
    data_root: &Path,
    database: &Path,
    project_id: &str,
    cwd: &str,
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
