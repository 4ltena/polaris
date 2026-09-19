//! Explicitly enabled project memory and pre-compaction archival.

use polaris_core::session::Session;
use polaris_memory::{MemoryStore, Record};
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use std::sync::Arc;

/// Serializes archival and forgetting across CLI/TUI processes.
pub fn lock_memory(state_dir: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(state_dir.join("memory.lock"))?;
    file.lock()?;
    Ok(file)
}

/// Where pre-compaction snapshots go, created private.
fn archive_base(state_dir: &Path, session_id: &str) -> io::Result<std::path::PathBuf> {
    let directory = state_dir.join("memory-archives");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory.join(format!("{session_id}.jsonl")))
}

fn valid_session_id(session_id: &str) -> io::Result<()> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid memory session id",
        ));
    }
    Ok(())
}

/// Keeps the pre-compaction history without building a search index.
///
/// The destructive path is `persist::rewrite`: on a legacy file log,
/// a successful compaction replaces the whole file with the summarised
/// list, so the original prefix then exists nowhere. A v2 session needs
/// none of this — its `raw.jsonl` is append-only and already holds every
/// message — so the caller installs this only when `persistence` is absent.
///
/// Until 2026-09-19 the snapshot and the searchable memory index were one
/// switch (`--remember`), which meant **not wanting a search index meant
/// losing the conversation.** Those are different decisions: one is about
/// recall, the other about not destroying what was said.
pub fn configure_archive(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_id: &str,
) -> io::Result<()> {
    valid_session_id(session_id)?;
    let project = polaris_core::project::resolve_root(project);
    let base = archive_base(state_dir, session_id)?;
    let state_dir = state_dir.to_path_buf();
    session.before_compact = Some(Arc::new(move |messages| {
        // The same lock the indexing hook takes, so two processes cannot
        // write one session's snapshot at once.
        let _lock = lock_memory(&state_dir)?;
        super::persist::archive_snapshot(&base, &project, messages)?;
        Ok(())
    }));
    Ok(())
}

pub fn configure_memory(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_id: &str,
) -> io::Result<()> {
    valid_session_id(session_id)?;
    let project = polaris_core::project::resolve_root(project);
    let project_id = super::persist::project_identity(&project)?;
    let archive_base = archive_base(state_dir, session_id)?;
    let database = state_dir.join("memory.sqlite3");
    let session_id = session_id.to_owned();
    let state_dir = state_dir.to_path_buf();
    session.before_compact = Some(Arc::new(move |messages| {
        let _lock = lock_memory(&state_dir)?;
        let mut store = MemoryStore::open(&database).map_err(io::Error::other)?;
        if store
            .is_session_forgotten(&project_id, &session_id)
            .map_err(io::Error::other)?
        {
            return Err(io::Error::other(
                "this session was forgotten; start a new session to archive",
            ));
        }
        let archive = super::persist::archive_snapshot(&archive_base, &project, messages)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis()
            .to_string();
        for (ordinal, message) in messages.iter().enumerate() {
            // Opaque encrypted reasoning is retained in the private archive, never indexed.
            if message.content.trim().is_empty() {
                continue;
            }
            let role = match message.role {
                polaris_provider::Role::User => "user",
                polaris_provider::Role::Assistant => "assistant",
                polaris_provider::Role::Tool => "tool",
            };
            let boundaries: Vec<usize> = message
                .content
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(message.content.len()))
                .collect();
            for start in (0..boundaries.len() - 1).step_by(2000) {
                let end = (start + 2000).min(boundaries.len() - 1);
                let text = &message.content[boundaries[start]..boundaries[end]];
                let mut hash = Sha256::new();
                hash.update(role.as_bytes());
                hash.update(ordinal.to_le_bytes());
                hash.update(start.to_le_bytes());
                hash.update(text.as_bytes());
                let id = format!("{:x}", hash.finalize());
                if store
                    .get(&project_id, &session_id, &id)
                    .map_err(io::Error::other)?
                    .is_some()
                {
                    continue;
                }
                store
                    .upsert(
                        &Record {
                            project_id: project_id.clone(),
                            session_id: session_id.clone(),
                            id,
                            role: role.into(),
                            timestamp: timestamp.clone(),
                            source: format!(
                                "{}#message={ordinal}&chars={start}-{end}",
                                archive.display()
                            ),
                            text: text.to_owned(),
                        },
                        None,
                    )
                    .map_err(io::Error::other)?;
            }
        }
        Ok(())
    }));
    Ok(())
}

pub(crate) fn saved_project_matches(
    session: &Session,
    project: &Path,
    session_path: &Path,
) -> bool {
    let root = polaris_core::project::resolve_root(project);
    match super::persist::read_meta(&session_path.with_extension("meta.json")) {
        Some(meta) => {
            Path::new(&meta.cwd).is_absolute()
                && polaris_core::project::resolve_root(Path::new(&meta.cwd)) == root
        }
        None => session.messages.is_empty(),
    }
}

/// A resumed conversation keeps its original project provenance.
pub fn configure_saved_memory(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_path: &Path,
) -> io::Result<()> {
    let root = polaris_core::project::resolve_root(project);
    let compatible = saved_project_matches(session, project, session_path);
    if !compatible {
        session.before_compact = Some(Arc::new(|_| {
            Err(io::Error::other(
                "元プロジェクトが異なるか不明なため、このセッションは記憶へ保存できません",
            ))
        }));
        return Ok(());
    }
    let id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::other("invalid session id"))?;
    configure_memory(session, &root, state_dir, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::Message;

    /// Archival without a search index. The compacted prefix has to
    /// survive for someone who never asked for recall — those are two
    /// decisions, and until 2026-09-19 one switch made both.
    #[test]
    fn the_archive_only_hook_keeps_the_originals_and_builds_no_index() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::default();
        configure_archive(&mut session, root.path(), state.path(), "plain-session").unwrap();

        let messages = vec![
            Message::user("これは圧縮で消えるはずだった発言"),
            Message::user("こちらも"),
        ];
        session.before_compact.as_ref().unwrap()(&messages).unwrap();

        let archive = state.path().join("memory-archives/plain-session.archive");
        let saved: Vec<_> = std::fs::read_dir(&archive)
            .expect("no archive directory")
            .flatten()
            .collect();
        assert_eq!(saved.len(), 1, "expected one snapshot: {saved:?}");
        let body = std::fs::read_to_string(saved[0].path()).unwrap();
        for message in &messages {
            assert!(
                body.contains(&message.content),
                "the snapshot lost {:?}",
                message.content
            );
        }
        // No index was asked for, so none was built. A sqlite file here
        // would mean the two decisions are still one.
        assert!(
            !state.path().join("memory.sqlite3").exists(),
            "an archive-only session built a search index"
        );
    }

    #[test]
    fn archives_and_indexes_original_but_forget_prevents_recreation() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::default();
        configure_memory(&mut session, root.path(), state.path(), "test-session").unwrap();
        let messages = vec![Message::user("保存した決定は青色です")];
        session.before_compact.as_ref().unwrap()(&messages).unwrap();
        let project_id = super::super::persist::project_identity(root.path()).unwrap();
        let mut store = MemoryStore::open(state.path().join("memory.sqlite3")).unwrap();
        assert_eq!(
            store
                .search(&polaris_memory::SearchRequest::lexical(&project_id, "青色"))
                .unwrap()
                .len(),
            1
        );
        store.delete_session(&project_id, "test-session").unwrap();
        let archive = state.path().join("memory-archives/test-session.archive");
        std::fs::remove_dir_all(&archive).unwrap();
        assert!(session.before_compact.as_ref().unwrap()(&messages).is_err());
        assert!(!archive.exists());
    }

    #[test]
    fn foreign_resume_cannot_enter_current_project_memory() {
        let current = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let path = logs.path().join("foreign.jsonl");
        let mut session = Session::new();
        session.push_user("他プロジェクトの記録");
        let meta =
            serde_json::json!({"cwd":foreign.path().to_str().unwrap(),"started_at_millis":0});
        // Unknown or foreign metadata must both prevent an attribution to current.
        std::fs::write(
            path.with_extension("meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        configure_saved_memory(&mut session, current.path(), logs.path(), &path).unwrap();
        assert!(session.before_compact.as_ref().unwrap()(&session.messages).is_err());
        assert!(!logs.path().join("memory.sqlite3").exists());
    }

    #[test]
    fn archive_and_forget_share_an_exclusive_lock() {
        let root = tempfile::tempdir().unwrap();
        let first = lock_memory(root.path()).unwrap();
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.path().join("memory.lock"))
            .unwrap();
        assert!(second.try_lock().is_err());
        drop(first);
        assert!(second.try_lock().is_ok());
    }
}
