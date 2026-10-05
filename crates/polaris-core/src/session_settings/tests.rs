//! Session-settings persistence, denial and recovery regression tests.

use super::*;
use crate::{
    conversation_state::{RewindEffectStatusV1, RewindEligibilityV1},
    management_effect::{
        ManagementEffectStateV1, ManagementEffectStoreV1, ManagementOperationIdV1,
    },
    project_mutation::{ProjectMutationLeaseV1, ProjectMutationOwnerV1},
};
use polaris_provider::Message;
use std::fs;

struct Fixture {
    _root: tempfile::TempDir,
    _mutation: ProjectMutationOwnerV1,
    lease: ProjectMutationLeaseV1,
    effects: ManagementEffectStoreV1,
    session: PersistedSession,
    initial: SavedSessionSettingsV1,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = project.join(".polaris");
        let data = root.path().join("data");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir(&data).unwrap();
        let mutation =
            ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = mutation.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let session = PersistedSession::create(
            &data,
            &data.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        Self {
            _root: root,
            _mutation: mutation,
            lease,
            effects,
            session,
            initial: SavedSessionSettingsV1::new("fake".into(), "model".into(), "auto".into())
                .unwrap(),
        }
    }

    fn owner(&self) -> SessionSettingsOwnerV1<'_> {
        SessionSettingsOwnerV1 {
            effects: &self.effects,
            lease: &self.lease,
        }
    }

    fn apply(&self, command: SessionSettingsCommandV1) -> ConversationSnapshot {
        let operation = ManagementOperationIdV1::random().unwrap();
        let published = self
            .owner()
            .apply(&self.session, &self.initial, command, operation)
            .unwrap();
        let effect = self.effects.effect(operation).unwrap().unwrap();
        assert!(matches!(
            effect.state,
            ManagementEffectStateV1::Completed { .. }
        ));
        published
    }

    fn refuse(&self, command: SessionSettingsCommandV1) {
        let before = self.session.snapshot().unwrap();
        assert!(SessionSettingsOwnerV1::preview(&self.session, &self.initial, command).is_err());
        assert_eq!(self.session.snapshot().unwrap().state, before.state);
    }
}

#[test]
fn m3c_settings_rows_publish_values_and_restore_without_replay() {
    let fixture = Fixture::new();
    let cursor_clone = fixture.session.clone();
    let snapshot = fixture.apply(SessionSettingsCommandV1::Remember { enabled: true });
    assert!(snapshot.state.session_settings.unwrap().remember);
    for mode in [
        ToolMemoryModeV1::History,
        ToolMemoryModeV1::Retrieval,
        ToolMemoryModeV1::Off,
    ] {
        let snapshot = fixture.apply(SessionSettingsCommandV1::ToolMemory { mode });
        assert_eq!(snapshot.state.session_settings.unwrap().tool_memory, mode);
    }
    let pair =
        SavedToolEmbeddingV1::new("http://localhost:8080/v1/".into(), "embed-1".into()).unwrap();
    assert_eq!(pair.url, "http://127.0.0.1:8080/v1/");
    let snapshot = fixture.apply(SessionSettingsCommandV1::EmbeddingEndpoint {
        embedding: Some(pair.clone()),
    });
    assert_eq!(
        snapshot
            .state
            .session_settings
            .unwrap()
            .tool_memory_embedding,
        Some(pair.clone())
    );
    let pair = SavedToolEmbeddingV1 {
        model: "embed-2".into(),
        ..pair
    };
    let snapshot = fixture.apply(SessionSettingsCommandV1::EmbeddingModel {
        embedding: Some(pair.clone()),
    });
    assert_eq!(
        snapshot
            .state
            .session_settings
            .unwrap()
            .tool_memory_embedding,
        Some(pair)
    );
    let snapshot = fixture.apply(SessionSettingsCommandV1::CompactThreshold {
        threshold: Some(1000),
    });
    assert_eq!(
        snapshot
            .state
            .session_settings
            .as_ref()
            .unwrap()
            .compact_threshold,
        Some(1000)
    );
    let revision = snapshot.state.session_settings.unwrap().revision;
    let snapshot = fixture.apply(SessionSettingsCommandV1::Configure {
        expected_revision: revision,
        provider: "fake".into(),
        model: "next-model".into(),
        effort: "auto".into(),
    });
    assert_eq!(snapshot.state.session_settings.unwrap().model, "next-model");
    fixture.apply(SessionSettingsCommandV1::Draft {
        expected_revision: 0,
        text: "未送信の本文".into(),
    });
    let expected = fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 0,
        operation: GoalOperationV1::Set {
            text: "完了まで進める".into(),
        },
    });
    assert_eq!(expected.raw_offset, 0);
    assert!(expected.events.is_empty());
    assert_eq!(cursor_clone.snapshot().unwrap().state, expected.state);
    let resumed = PersistedSession::open(
        &fixture.session.data_root,
        &fixture.session.database,
        &expected.state.project_id,
        &expected.state.session_id,
    )
    .unwrap();
    assert_eq!(resumed.snapshot().unwrap().state, expected.state);
    assert_eq!(fixture.effects.effects().unwrap().len(), 10);
    cursor_clone
        .append(Message::user("次の入力"), true, None)
        .unwrap();
    assert_eq!(
        fixture.session.snapshot().unwrap().state.generation,
        expected.state.generation + 1
    );
}

#[test]
fn m3c_settings_rows_refuse_cancel_foreign_lease_and_invalid_values() {
    let fixture = Fixture::new();
    let before = fixture.session.snapshot().unwrap();
    let preview = SessionSettingsOwnerV1::preview(
        &fixture.session,
        &fixture.initial,
        SessionSettingsCommandV1::Remember { enabled: true },
    )
    .unwrap();
    let prepared = fixture
        .owner()
        .prepare(
            &fixture.session,
            ManagementOperationIdV1::random().unwrap(),
            &preview,
        )
        .unwrap();
    fixture.owner().deny(&prepared).unwrap();
    assert_eq!(fixture.session.snapshot().unwrap().state, before.state);
    let other = tempfile::tempdir().unwrap();
    let foreign_data = other.path().join("data");
    fs::create_dir(&foreign_data).unwrap();
    let foreign_session = PersistedSession::create(
        &foreign_data,
        &foreign_data.join("memory.sqlite3"),
        other.path().to_str().unwrap(),
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let preview = SessionSettingsOwnerV1::preview(
        &foreign_session,
        &fixture.initial,
        SessionSettingsCommandV1::Remember { enabled: true },
    )
    .unwrap();
    let effect_count = fixture.effects.effects().unwrap().len();
    assert!(
        fixture
            .owner()
            .prepare(
                &foreign_session,
                ManagementOperationIdV1::random().unwrap(),
                &preview
            )
            .is_err()
    );
    assert_eq!(foreign_session.snapshot().unwrap().state.generation, 0);
    assert_eq!(fixture.effects.effects().unwrap().len(), effect_count);

    let other_owner =
        ProjectMutationOwnerV1::at_store_root(&other.path().join("rewind-workspace")).unwrap();
    let other_lease = other_owner.try_mutation().unwrap();
    let foreign = SessionSettingsOwnerV1 {
        effects: &fixture.effects,
        lease: &other_lease,
    };
    let preview = SessionSettingsOwnerV1::preview(
        &fixture.session,
        &fixture.initial,
        SessionSettingsCommandV1::ToolMemory {
            mode: ToolMemoryModeV1::History,
        },
    )
    .unwrap();
    assert!(
        foreign
            .prepare(
                &fixture.session,
                ManagementOperationIdV1::random().unwrap(),
                &preview
            )
            .is_err()
    );
    assert_eq!(fixture.session.snapshot().unwrap().state, before.state);
    fixture.refuse(SessionSettingsCommandV1::EmbeddingEndpoint {
        embedding: Some(SavedToolEmbeddingV1 {
            url: "https://example.com/v1".into(),
            model: "embed".into(),
        }),
    });
    fixture.refuse(SessionSettingsCommandV1::EmbeddingModel {
        embedding: Some(SavedToolEmbeddingV1 {
            url: "http://127.0.0.1:8080/v1".into(),
            model: String::new(),
        }),
    });
    fixture.refuse(SessionSettingsCommandV1::CompactThreshold {
        threshold: Some(999),
    });
    fixture.refuse(SessionSettingsCommandV1::Configure {
        expected_revision: 0,
        provider: "another-provider".into(),
        model: "model".into(),
        effort: "auto".into(),
    });
    fixture.refuse(SessionSettingsCommandV1::Configure {
        expected_revision: 0,
        provider: "fake".into(),
        model: "model".into(),
        effort: "high".into(),
    });
}

#[test]
fn m3c_history_initializes_only_proven_empty_sessions() {
    let fixture = Fixture::new();
    let snapshot = fixture.apply(SessionSettingsCommandV1::History {
        mode: HistoryMode::Strict10,
    });
    assert_eq!(snapshot.state.history_mode, HistoryMode::Strict10);
    let snapshot = fixture.apply(SessionSettingsCommandV1::History {
        mode: HistoryMode::Structured10,
    });
    assert_eq!(snapshot.state.history_mode, HistoryMode::Structured10);
    let memory = polaris_memory::MemoryStore::open(&fixture.session.database).unwrap();
    assert_eq!(
        memory.structured_memory_schema_version().unwrap(),
        Some(polaris_memory::structured::STRUCTURED_MEMORY_SCHEMA_VERSION)
    );
    drop(memory);
    fixture
        .session
        .append(Message::user("original history"), true, None)
        .unwrap();
    fixture.refuse(SessionSettingsCommandV1::History {
        mode: HistoryMode::Legacy,
    });
    fixture.session.clear().unwrap();
    fixture.refuse(SessionSettingsCommandV1::History {
        mode: HistoryMode::Legacy,
    });
    let before = fixture.session.snapshot().unwrap();
    let mut direct = before.state.clone();
    direct.history_mode = HistoryMode::Legacy;
    assert!(
        fixture
            .session
            .store
            .lock()
            .unwrap()
            .publish(&before, direct)
            .is_err()
    );
}

#[test]
fn m3c_draft_preserves_text_clears_blank_and_refuses_stale_or_oversize() {
    let fixture = Fixture::new();
    let snapshot = fixture.apply(SessionSettingsCommandV1::Draft {
        expected_revision: 0,
        text: "  原文\n入力  ".into(),
    });
    assert_eq!(snapshot.state.draft.text, "  原文\n入力  ");
    assert!(snapshot.events.is_empty());
    fixture.refuse(SessionSettingsCommandV1::Draft {
        expected_revision: 0,
        text: "stale".into(),
    });
    fixture.refuse(SessionSettingsCommandV1::Draft {
        expected_revision: 1,
        text: "x".repeat(MAX_DRAFT_BYTES + 1),
    });
    let snapshot = fixture.apply(SessionSettingsCommandV1::Draft {
        expected_revision: 1,
        text: " \t\n".into(),
    });
    assert_eq!(snapshot.state.draft.revision, 2);
    assert!(snapshot.state.draft.text.is_empty());
}

#[test]
fn m3c_goal_uses_exact_revision_and_preserves_stop_until_resume() {
    let fixture = Fixture::new();
    let snapshot = fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 0,
        operation: GoalOperationV1::Set {
            text: "first goal".into(),
        },
    });
    assert!(snapshot.state.goal.unwrap().active);
    fixture.refuse(SessionSettingsCommandV1::Goal {
        expected_revision: 0,
        operation: GoalOperationV1::Clear,
    });
    fixture.refuse(SessionSettingsCommandV1::Goal {
        expected_revision: 1,
        operation: GoalOperationV1::Set { text: "   ".into() },
    });
    fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 1,
        operation: GoalOperationV1::Stop,
    });
    let snapshot = fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 2,
        operation: GoalOperationV1::Set {
            text: "next goal".into(),
        },
    });
    assert!(snapshot.state.goal.unwrap().stopped);
    let snapshot = fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 3,
        operation: GoalOperationV1::Resume,
    });
    assert!(!snapshot.state.goal.unwrap().stopped);
    let snapshot = fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 4,
        operation: GoalOperationV1::Clear,
    });
    let goal = snapshot.state.goal.unwrap();
    assert_eq!(goal.revision.get(), 5);
    assert!(!goal.active);
}

#[test]
fn m3c_rewind_restores_captured_goal_and_starts_with_independent_blank_draft() {
    let fixture = Fixture::new();
    fixture.apply(SessionSettingsCommandV1::Remember { enabled: true });
    fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 0,
        operation: GoalOperationV1::Set {
            text: "captured goal".into(),
        },
    });
    fixture.apply(SessionSettingsCommandV1::Draft {
        expected_revision: 0,
        text: "independent draft".into(),
    });
    let boundary = fixture
        .session
        .begin_rewind_boundary("a".repeat(32), Vec::new())
        .unwrap();
    fixture
        .session
        .append(Message::user("real input"), true, None)
        .unwrap();
    fixture
        .session
        .append(Message::assistant("answer"), false, None)
        .unwrap();
    fixture
        .session
        .finish_rewind_boundary(
            &boundary.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
    fixture.apply(SessionSettingsCommandV1::Goal {
        expected_revision: 1,
        operation: GoalOperationV1::Set {
            text: "later goal".into(),
        },
    });
    let lock = fixture.session.store.lock().unwrap();
    let branch = lock
        .prepare_rewind_branch(
            &fixture.session.data_root,
            &boundary.boundary_id,
            "00000000-0000-4000-8000-000000000002",
        )
        .unwrap();
    let restored = branch.lock().unwrap().snapshot().unwrap();
    assert_eq!(restored.state.goal.unwrap().text, "captured goal");
    assert_eq!(restored.state.draft, SavedSessionDraftV1::default());
    assert!(restored.state.session_settings.unwrap().remember);
    assert!(restored.events.is_empty());
}

#[test]
fn m3c_publication_refuses_non_session_fields_and_full_preimage_mismatch() {
    let fixture = Fixture::new();
    let before = fixture.session.snapshot().unwrap();
    let hash = content_hash(&serde_json::to_vec(&before.state).unwrap());
    let mut candidate = before.state.clone();
    candidate
        .visible_summary_ids
        .push("unauthorized-summary".into());
    assert!(
        fixture
            .session
            .publish_session_change(before.state.generation, &hash, candidate)
            .is_err()
    );
    assert!(
        fixture
            .session
            .publish_session_change(
                before.state.generation,
                &"a".repeat(64),
                before.state.clone()
            )
            .is_err()
    );
    assert_eq!(fixture.session.snapshot().unwrap().state, before.state);
}
