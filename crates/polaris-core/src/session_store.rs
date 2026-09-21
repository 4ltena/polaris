//! Durable session attachment, generation checks, and tombstone-serialized writes.

use crate::{
    conversation_memory::published_view,
    conversation_state::{
        ConversationSnapshot, ConversationStateV2, ConversationStore, HistoryMode,
    },
    workflow::{SessionWorkflow, WorkflowConfig, WorkflowMode},
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
            state.workflow_config = Some(
                crate::conversation_state::SavedWorkflowConfigV1::capture(&workflow.config),
            );
            state.workflow = workflow.state();
            state.gates = workflow.gates.clone();
        }
        let mut memory = MemoryStore::open(database).map_err(io::Error::other)?;
        // A structured session is saved only after its index migration reads back.
        if mode == HistoryMode::Structured10 {
            memory
                .enable_structured_memory()
                .map_err(io::Error::other)?;
            if memory
                .structured_memory_schema_version()
                .map_err(io::Error::other)?
                != Some(polaris_memory::structured::STRUCTURED_MEMORY_SCHEMA_VERSION)
            {
                return Err(io::Error::other(
                    "structured memory migration did not read back",
                ));
            }
        }
        drop(memory);
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
        let initial = store.lock()?.snapshot()?;
        let generation = initial.state.generation;
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
        self.update(&[(message, starts_turn)], workflow).map(|_| ())
    }

    /// Return the identity produced by the same locked append/publication.
    pub(crate) fn append_input(
        &self,
        message: Message,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        self.update(&[(message, true)], workflow)
    }

    /// Publishes a completed assistant/tool suffix and its workflow checkpoint
    /// under one writer lock and one state marker. Batch messages are never a
    /// new real user turn.
    pub fn append_batch(
        &self,
        messages: &[Message],
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<()> {
        let events: Vec<_> = messages
            .iter()
            .cloned()
            .map(|message| (message, false))
            .collect();
        self.update(&events, workflow).map(|_| ())
    }

    pub fn checkpoint(&self, workflow: Option<&SessionWorkflow>) -> io::Result<()> {
        self.update(&[], workflow).map(|_| ())
    }

    fn update(
        &self,
        events: &[(Message, bool)],
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor {
            return Err(io::Error::other(
                "保存世代が変わりました。原文を保持して停止しました",
            ));
        }
        if events.is_empty()
            && before.state.raw_commit_hash == before.raw_hash
            && before.state.raw_commit_offset == before.raw_offset
            && workflow.is_none_or(|workflow| {
                before.state.workflow == workflow.state()
                    && before.state.gates == workflow.gates
                    && before
                        .state
                        .workflow_config
                        .as_ref()
                        .is_some_and(|config| config.config == workflow.config)
            })
        {
            let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
            return memory
                .with_conversation_marker(&published_view(&before.state)?, || Ok(()))
                .map(|_| None)
                .map_err(io::Error::other);
        }
        let mut memory = MemoryStore::open(&self.database).map_err(io::Error::other)?;
        // The old writer's forget is serialized with both raw append and marker.
        let mut source = None;
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                crate::conversation_memory::reject_unpublished_structured_attempt(
                    &self.store,
                    &before,
                )?;
                let mut snapshot = before.clone();
                for (message, starts_turn) in events {
                    snapshot = lock.append(message.clone(), *starts_turn)?;
                }
                let mut next = snapshot.state.clone();
                if let Some(workflow) = workflow {
                    next.workflow_config = Some(
                        crate::conversation_state::SavedWorkflowConfigV1::capture(&workflow.config),
                    );
                    next.workflow = workflow.state();
                    next.gates = workflow.gates.clone();
                }
                lock.publish(&snapshot, next)?;
                source = events
                    .last()
                    .filter(|(_, starts_turn)| *starts_turn)
                    .and_then(|_| {
                        snapshot.events.last().and_then(|event| {
                            crate::request_input::SourceBinding::from_event(
                                &snapshot.state.session_id,
                                event,
                            )
                        })
                    });
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = before.state.generation + 1;
        Ok(source)
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
        let events: Vec<_> = messages
            .iter()
            .cloned()
            .map(|message| {
                let starts_turn = message.role == Role::User;
                (message, starts_turn)
            })
            .collect();
        this.update(&events, workflow)?;
        Ok(this)
    }

    pub fn restore_workflow(&self, config: WorkflowConfig) -> io::Result<SessionWorkflow> {
        let (workflow, needs_checkpoint) = self.prepare_workflow(config)?;
        if needs_checkpoint {
            self.checkpoint(Some(&workflow))?;
        }
        Ok(workflow)
    }

    /// Restore without writing so callers can validate explicit overrides first.
    /// The flag requests a checkpoint for a missing saved configuration.
    pub fn prepare_workflow(
        &self,
        mut config: WorkflowConfig,
    ) -> io::Result<(SessionWorkflow, bool)> {
        let state = self.snapshot()?.state;
        let mut gates = state.gates;
        // Resume has not verified that the working files still match evidence.
        // A new explicit verification can replace this conservative stale flag.
        if let Some(verification) = &mut gates.verification {
            verification.stale = true;
        }
        let needs_checkpoint = state.workflow_config.is_none();
        let config = match state.workflow_config {
            Some(saved) => saved.config,
            None => {
                config.mode = WorkflowMode::Manual;
                config
            }
        };
        let workflow =
            SessionWorkflow::restore(config, gates, state.workflow).map_err(io::Error::other)?;
        Ok((workflow, needs_checkpoint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn workflow(config: WorkflowConfig) -> SessionWorkflow {
        SessionWorkflow::new(config)
    }

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
    fn every_history_mode_can_be_created_and_resumed_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("memory.sqlite3");
        for mode in [
            HistoryMode::Legacy,
            HistoryMode::Strict10,
            HistoryMode::Structured10,
        ] {
            let saved =
                PersistedSession::create(dir.path(), &database, "project", mode, None).unwrap();
            let id = saved.snapshot().unwrap().state.session_id;
            let reopened = PersistedSession::open(dir.path(), &database, "project", &id).unwrap();
            assert_eq!(reopened.snapshot().unwrap().state.history_mode, mode);
        }
        let broken = tempfile::tempdir().unwrap();
        let database = broken.path().join("memory.sqlite3");
        MemoryStore::open(&database).unwrap();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TABLE memory_schema_meta (component TEXT PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL);
                 INSERT INTO memory_schema_meta VALUES ('structured_memory', 99);",
            )
            .unwrap();
        assert!(
            PersistedSession::create(
                broken.path(),
                &database,
                "project",
                HistoryMode::Structured10,
                None
            )
            .is_err()
        );
        assert!(!broken.path().join("sessions-v2").exists(), "nothing saved");
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
    fn saved_workflow_config_is_authoritative_for_resume_and_fork() {
        let dir = tempfile::tempdir().unwrap();
        let saved_config = WorkflowConfig {
            enabled: false,
            mode: WorkflowMode::Manual,
            ..Default::default()
        };
        let saved_workflow = workflow(saved_config.clone());
        let saved = PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            Some(&saved_workflow),
        )
        .unwrap();
        let mut changed_config = saved_config.clone();
        changed_config.enabled = true;
        changed_config.mode = WorkflowMode::Adaptive;
        changed_config.initial_phase = crate::workflow::Phase::Review;

        let resumed = saved.restore_workflow(changed_config.clone()).unwrap();
        assert_eq!(resumed.config, saved_config);
        assert!(!resumed.config.enabled);

        let child = saved.fork(&"a".repeat(64)).unwrap();
        let forked = child.restore_workflow(changed_config).unwrap();
        assert_eq!(forked.config, saved_config);
    }

    #[test]
    fn legacy_workflow_config_defaults_to_manual_and_is_checkpointed_once() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let current = WorkflowConfig {
            enabled: false,
            mode: WorkflowMode::Adaptive,
            initial_phase: crate::workflow::Phase::Implement,
            ..Default::default()
        };

        let restored = saved.restore_workflow(current.clone()).unwrap();
        assert_eq!(restored.config.mode, WorkflowMode::Manual);
        assert!(!restored.config.enabled);
        let first = saved.snapshot().unwrap();
        assert_eq!(first.state.generation, 1);
        assert_eq!(
            first.state.workflow_config.unwrap().config.mode,
            WorkflowMode::Manual
        );

        let again = saved.restore_workflow(current).unwrap();
        assert_eq!(again.config.mode, WorkflowMode::Manual);
        assert_eq!(saved.snapshot().unwrap().state.generation, 1);
    }

    #[test]
    fn unknown_saved_workflow_config_schema_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let path = saved.store.directory().join("state.json");
        let mut state: ConversationStateV2 =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state.workflow_config = Some(crate::conversation_state::SavedWorkflowConfigV1 {
            schema_version: 99,
            config: WorkflowConfig::default(),
        });
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(saved.restore_workflow(WorkflowConfig::default()).is_err());
    }

    #[test]
    fn append_batch_publishes_suffix_and_workflow_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = WorkflowConfig::default();
        let workflow = workflow(config);
        let saved = PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        saved
            .append(Message::user("turn"), true, Some(&workflow))
            .unwrap();
        let before = saved.snapshot().unwrap().state.generation;
        saved
            .append_batch(
                &[Message::assistant("answer"), Message::assistant("detail")],
                Some(&workflow),
            )
            .unwrap();
        let after = saved.snapshot().unwrap();
        assert_eq!(after.state.generation, before + 1);
        assert_eq!(after.events.len(), 3);
        assert!(after.events[1..].iter().all(|event| !event.starts_turn));
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
