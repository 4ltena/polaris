//! Durable session attachment, generation checks, and tombstone-serialized writes.

use crate::{
    conversation_memory::published_view,
    conversation_state::{
        ConversationSnapshot, ConversationStateV2, ConversationStore, HistoryMode,
    },
    workflow::{SessionWorkflow, WorkflowConfig},
};
use polaris_memory::MemoryStore;
use polaris_provider::{Message, Role};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Clones used by the UI's running turn share a cursor. A separately resumed
/// writer gets its own cursor and cannot silently overwrite a newer generation.
#[derive(Clone)]
pub struct PersistedSession {
    pub store: Arc<ConversationStore>,
    pub database: PathBuf,
    pub data_root: PathBuf,
    generation: Arc<Mutex<u64>>,
}

pub fn new_session_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

impl PersistedSession {
    pub fn create(
        data_root: &Path,
        database: &Path,
        project_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Self> {
        let mut state = ConversationStateV2::new(project_id.into(), new_session_id()?)?;
        state.history_mode = mode;
        if let Some(workflow) = workflow {
            state.workflow = workflow.state();
            state.gates = workflow.gates.clone();
        }
        MemoryStore::open(database).map_err(io::Error::other)?;
        let store = ConversationStore::create(data_root, state)?;
        Ok(Self {
            store: Arc::new(store),
            database: database.into(),
            data_root: data_root.into(),
            generation: Arc::new(Mutex::new(0)),
        })
    }

    pub fn open(data_root: &Path, database: &Path, project_id: &str, id: &str) -> io::Result<Self> {
        let store = ConversationStore::open(data_root, project_id, id)?;
        let generation = store.lock()?.snapshot()?.state.generation;
        let this = Self {
            store: Arc::new(store),
            database: database.into(),
            data_root: data_root.into(),
            generation: Arc::new(Mutex::new(generation)),
        };
        this.snapshot()?;
        Ok(this)
    }

    fn cursor(&self) -> io::Result<std::sync::MutexGuard<'_, u64>> {
        self.generation
            .lock()
            .map_err(|_| io::Error::other("保存カーソルを取得できません"))
    }

    pub fn snapshot(&self) -> io::Result<ConversationSnapshot> {
        let cursor = self.cursor()?;
        let lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other(
                "別の書き込みでセッションが更新されました。再開し直してください",
            ));
        }
        let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
        memory
            .with_conversation_marker(&published_view(&snapshot.state)?, || Ok(()))
            .map_err(io::Error::other)?;
        Ok(snapshot)
    }

    /// Accept only the exact snapshot produced by our async history operation.
    /// No locks are retained across the caller's model/embedding await.
    pub fn accept_prepared(&self, expected: &ConversationSnapshot) -> io::Result<()> {
        self.accept_owned_snapshot(expected, false)
    }

    pub fn accept_publication(&self, expected: &ConversationSnapshot) -> io::Result<()> {
        self.accept_owned_snapshot(expected, true)
    }

    fn accept_owned_snapshot(
        &self,
        expected: &ConversationSnapshot,
        published: bool,
    ) -> io::Result<()> {
        let mut cursor = self.cursor()?;
        let owned = if published {
            cursor
                .checked_add(1)
                .ok_or_else(|| io::Error::other("保存世代が上限です"))?
        } else {
            *cursor
        };
        if expected.state.generation != owned {
            return Err(io::Error::other("要求準備中に別の書き込みが発生しました"));
        }
        let lock = self.store.lock()?;
        lock.check_snapshot(expected)?;
        let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
        memory
            .with_conversation_marker(&published_view(&expected.state)?, || Ok(()))
            .map_err(io::Error::other)?;
        *cursor = owned;
        Ok(())
    }

    pub fn append(
        &self,
        message: Message,
        starts_turn: bool,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<()> {
        self.update(Some((message, starts_turn)), workflow)
    }

    pub fn checkpoint(&self, workflow: Option<&SessionWorkflow>) -> io::Result<()> {
        self.update(None, workflow)
    }

    fn update(
        &self,
        event: Option<(Message, bool)>,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<()> {
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor {
            return Err(io::Error::other(
                "保存世代が変わりました。原文を保持して停止しました",
            ));
        }
        if event.is_none()
            && before.state.raw_commit_hash == before.raw_hash
            && before.state.raw_commit_offset == before.raw_offset
            && workflow.is_none_or(|workflow| {
                before.state.workflow == workflow.state() && before.state.gates == workflow.gates
            })
        {
            let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
            return memory
                .with_conversation_marker(&published_view(&before.state)?, || Ok(()))
                .map_err(io::Error::other);
        }
        let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
        // The old writer's forget is serialized with both raw append and marker.
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                let snapshot = match event {
                    Some((message, starts_turn)) => lock.append(message, starts_turn)?,
                    None => before.clone(),
                };
                let mut next = snapshot.state.clone();
                if let Some(workflow) = workflow {
                    next.workflow = workflow.state();
                    next.gates = workflow.gates.clone();
                }
                lock.publish(&snapshot, next)?;
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = before.state.generation + 1;
        Ok(())
    }

    pub fn clear(&self) -> io::Result<()> {
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                lock.clear_epoch()?;
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = before.state.generation + 1;
        Ok(())
    }

    pub fn fork(&self, artifact_hash: &str) -> io::Result<Self> {
        let cursor = self.cursor()?;
        let lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
        let mut child = None;
        memory
            .with_conversation_marker(&published_view(&snapshot.state)?, || {
                child = Some(lock.fork(&self.data_root, &new_session_id()?, artifact_hash)?);
                Ok(())
            })
            .map_err(io::Error::other)?;
        let store = child.ok_or_else(|| io::Error::other("分岐を保存できません"))?;
        let generation = store.lock()?.snapshot()?.state.generation;
        Ok(Self {
            store: Arc::new(store),
            database: self.database.clone(),
            data_root: self.data_root.clone(),
            generation: Arc::new(Mutex::new(generation)),
        })
    }

    /// Import into a fresh UUID; the caller's legacy JSONL remains untouched.
    pub fn import(
        data_root: &Path,
        database: &Path,
        project_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
        messages: &[Message],
    ) -> io::Result<Self> {
        let this = Self::create(data_root, database, project_id, mode, workflow)?;
        for message in messages {
            this.append(message.clone(), message.role == Role::User, workflow)?;
        }
        Ok(this)
    }

    pub fn restore_workflow(&self, config: WorkflowConfig) -> io::Result<SessionWorkflow> {
        let state = self.snapshot()?.state;
        let mut gates = state.gates;
        // Resume has not verified that the working files still match evidence.
        // A new explicit verification can replace this conservative stale flag.
        if let Some(verification) = &mut gates.verification {
            verification.stale = true;
        }
        SessionWorkflow::restore(config, gates, state.workflow).map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn create(root: &Path) -> PersistedSession {
        PersistedSession::create(
            root,
            &root.join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
        )
        .unwrap()
    }

    #[test]
    fn resumed_partial_tool_batch_is_closed_without_reexecution() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let mut session = crate::session::Session::new();
        session.attach(saved.clone()).unwrap();
        session.push_user("work");
        session.push_assistant_tool_calls(
            "",
            vec![
                polaris_provider::ToolCall {
                    id: "a".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                },
                polaris_provider::ToolCall {
                    id: "b".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            Vec::new(),
        );
        session.push_tool_result("a", "completed");
        session.messages.truncate(1);
        session.recover_interrupted_tools().unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[1].tool_calls.len(), 2);
        let mut resumed = crate::session::Session::new();
        resumed.attach(saved.clone()).unwrap();
        resumed.push_user("continue");
        resumed.check_persistence().unwrap();
        let raw = saved.snapshot().unwrap();
        assert_eq!(raw.events.len(), 5);
        assert_eq!(raw.events[3].message.tool_call_id.as_deref(), Some("b"));
        assert!(raw.events[3].message.content.contains("結果は不明"));
        assert_eq!(raw.events[4].message.content, "continue");
        assert_eq!(raw.events[2].message.content, "completed");
    }

    #[test]
    fn separately_resumed_writer_cannot_append_after_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let first = create(dir.path());
        let id = first.snapshot().unwrap().state.session_id;
        let second = PersistedSession::open(dir.path(), &first.database, "project", &id).unwrap();
        first.append(Message::user("first"), true, None).unwrap();
        assert!(second.append(Message::user("stale"), true, None).is_err());
        assert_eq!(first.snapshot().unwrap().events.len(), 1);
    }

    #[test]
    fn forgotten_session_rejects_raw_append_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let id = saved.snapshot().unwrap().state.session_id;
        let raw = saved.store.directory().join("raw.jsonl");
        let mut session = Session::new();
        session.attach(saved.clone()).unwrap();
        session.push_user("retained");
        let original = std::fs::read(&raw).unwrap();
        MemoryStore::open(&saved.database)
            .unwrap()
            .delete_session("project", &id)
            .unwrap();
        session.push_assistant("must not persist", Vec::new());
        assert!(session.check_persistence().is_err());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(std::fs::read(&raw).unwrap(), original);
        assert!(PersistedSession::open(dir.path(), &saved.database, "project", &id).is_err());
    }

    #[test]
    fn unchanged_checkpoint_does_not_invalidate_pending_generation() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        saved.append(Message::user("turn"), true, None).unwrap();
        let expected = saved.snapshot().unwrap();
        saved.checkpoint(None).unwrap();
        assert_eq!(saved.snapshot().unwrap().state, expected.state);
    }

    #[test]
    fn clear_and_fork_preserve_raw_but_isolate_future_history() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        saved.append(Message::user("first"), true, None).unwrap();
        saved
            .append(Message::assistant("answer"), false, None)
            .unwrap();
        let child = saved.fork(&"a".repeat(64)).unwrap();
        saved.clear().unwrap();
        saved
            .append(Message::user("second epoch"), true, None)
            .unwrap();
        let snapshot = saved.snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.recent_messages().len(), 1);
        assert_eq!(child.snapshot().unwrap().events.len(), 2);
        assert_ne!(
            snapshot.state.session_id,
            child.snapshot().unwrap().state.session_id
        );
    }

    #[test]
    fn legacy_import_does_not_mutate_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let messages = vec![Message::user("legacy"), Message::assistant("answer")];
        let path = dir.path().join("old.jsonl");
        let source = serde_json::to_vec(&messages).unwrap();
        std::fs::write(&path, &source).unwrap();
        let saved = PersistedSession::import(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
            &messages,
        )
        .unwrap();
        assert_eq!(saved.snapshot().unwrap().events.len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), source);
    }
}
