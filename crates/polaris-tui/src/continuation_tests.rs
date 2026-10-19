//! Shared continuation recovery through real raw/receipt owners and fake backends.
use super::*;
use polaris_core::{
    conversation_memory::{
        EmbeddingModel, StrictEmbedder, StrictHistory, StrictSummaryProvider, SummaryRequest,
    },
    conversation_state::HistoryMode,
    session::Session,
    session_store::PersistedSession,
    workflow::{SessionWorkflow, WorkflowConfig},
};
use polaris_provider::Message;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

type StrictFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<T>> + Send + 'a>>;

struct Summary {
    calls: AtomicUsize,
    fail: bool,
}
impl StrictSummaryProvider for Summary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(std::io::Error::other("fixture provider failure"));
            }
            let input = request.structured_sources.unwrap();
            Ok(serde_json::json!({
                "summary":{"facts":["saved fact"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[request.source_turn_id]},
                "processed_source_ids":input.extraction_source_ids,
                "entity_candidates":[],"fact_deltas":[]
            }).to_string())
        })
    }
}

struct Embedder {
    metadata: EmbeddingModel,
    calls: AtomicUsize,
    fail: AtomicBool,
}
impl StrictEmbedder for Embedder {
    fn metadata(&self) -> &EmbeddingModel {
        &self.metadata
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(std::io::Error::other("fixture embedding failure"))
            } else {
                Ok(vec![1.0, 0.0])
            }
        })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async { panic!("continuation must not embed a query") })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    saved: PersistedSession,
    history: Arc<StrictHistory>,
    summary: Arc<Summary>,
    embedder: Arc<Embedder>,
}
fn fixture(workflow: Option<&SessionWorkflow>, provider_failure: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let saved = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "p",
        HistoryMode::Strict10,
        workflow,
    )
    .unwrap();
    // Seed the mode on an existing session, then keep the production owner and
    // generation checks.
    let mut initial = saved.snapshot().unwrap().state;
    initial.history_mode = HistoryMode::Structured10;
    std::fs::write(
        saved.store.directory().join("state.json"),
        serde_json::to_vec(&initial).unwrap(),
    )
    .unwrap();
    let mut lock = saved.store.lock().unwrap();
    for turn in 1..=11 {
        lock.append(Message::user(format!("fixture turn {turn}")), true)
            .unwrap();
    }
    let snapshot = lock.snapshot().unwrap();
    lock.publish(&snapshot, snapshot.state.clone()).unwrap();
    let snapshot = lock.snapshot().unwrap();
    drop(lock);
    saved.accept_publication(&snapshot).unwrap();
    let summary = Arc::new(Summary {
        calls: AtomicUsize::new(0),
        fail: provider_failure,
    });
    let embedder = Arc::new(Embedder {
        metadata: EmbeddingModel {
            model: "fixture".into(),
            revision: "r33".into(),
            dimension: 2,
        },
        calls: AtomicUsize::new(0),
        fail: AtomicBool::new(true),
    });
    let history = Arc::new(StrictHistory::new(summary.clone(), embedder.clone()));
    Fixture {
        _root: root,
        saved,
        history,
        summary,
        embedder,
    }
}

#[tokio::test]
async fn r33_saved_response_is_recovered_before_missing_workflow_is_checkpointed() {
    let fixture = fixture(None, false);
    assert!(
        fixture
            .history
            .recover_pending(&fixture.saved.store, &fixture.saved.database)
            .await
            .is_err()
    );
    let before = fixture.saved.snapshot().unwrap();
    let raw = std::fs::read(fixture.saved.store.directory().join("raw.jsonl")).unwrap();
    fixture.embedder.fail.store(false, Ordering::SeqCst);
    let mut session = Session {
        strict_history: Some(fixture.history.clone()),
        ..Session::default()
    };
    restore_prepared_continuation(
        &mut session,
        &PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence: fixture.saved.clone(),
        },
        &WorkflowConfig::default(),
        false,
        &[],
        None,
    )
    .await
    .unwrap();
    let after = fixture.saved.snapshot().unwrap();
    assert_eq!(after.state.generation, before.state.generation + 2);
    assert_eq!(after.state.visible_structured_publication_ids.len(), 1);
    assert!(after.state.workflow_config.is_some());
    assert_eq!(
        std::fs::read(fixture.saved.store.directory().join("raw.jsonl")).unwrap(),
        raw
    );
    assert_eq!(session.messages.len(), 11);
    assert_eq!(fixture.summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.embedder.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn r33_unknown_response_stops_before_saved_workflow_or_window_changes() {
    let workflow = SessionWorkflow::new(WorkflowConfig::default());
    let fixture = fixture(Some(&workflow), true);
    assert!(
        fixture
            .history
            .recover_pending(&fixture.saved.store, &fixture.saved.database)
            .await
            .is_err()
    );
    let before = fixture.saved.snapshot().unwrap();
    let raw = std::fs::read(fixture.saved.store.directory().join("raw.jsonl")).unwrap();
    let mut session = Session {
        strict_history: Some(fixture.history.clone()),
        ..Session::default()
    };
    session.push_user("current window");
    let result = restore_prepared_continuation(
        &mut session,
        &PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence: fixture.saved.clone(),
        },
        &WorkflowConfig::default(),
        false,
        &[],
        None,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(fixture.saved.snapshot().unwrap().state, before.state);
    assert_eq!(
        std::fs::read(fixture.saved.store.directory().join("raw.jsonl")).unwrap(),
        raw
    );
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "current window");
    assert!(session.persistence.is_none());
    assert_eq!(fixture.summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.embedder.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r33_legacy_continuation_does_not_construct_an_unused_backend() {
    let root = tempfile::tempdir().unwrap();
    let saved = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "p",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let factory: HistoryBackendFactory = Arc::new(|| panic!("legacy must not construct a backend"));
    let mut session = Session::default();
    restore_prepared_continuation(
        &mut session,
        &PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence: saved,
        },
        &WorkflowConfig::default(),
        false,
        &[],
        Some(&factory),
    )
    .await
    .unwrap();
    assert!(session.strict_history.is_none());
    assert!(session.summary_usage.is_none());
}

#[tokio::test]
async fn r33_strict_continuation_installs_backend_once_and_preserves_it_on_retry() {
    for mode in [HistoryMode::Strict10, HistoryMode::Structured10] {
        let fixture = fixture(None, false);
        let saved = if mode == HistoryMode::Strict10 {
            PersistedSession::create(
                fixture._root.path(),
                &fixture.saved.database,
                "p",
                mode,
                None,
            )
            .unwrap()
        } else {
            assert!(
                fixture
                    .history
                    .recover_pending(&fixture.saved.store, &fixture.saved.database)
                    .await
                    .is_err()
            );
            fixture.embedder.fail.store(false, Ordering::SeqCst);
            fixture.saved.clone()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let history = fixture.history.clone();
        let factory_calls = calls.clone();
        let factory: HistoryBackendFactory = Arc::new(move || {
            factory_calls.fetch_add(1, Ordering::SeqCst);
            Ok((history.clone(), polaris_provider::UsageMeter::default()))
        });
        let continuation = PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence: saved,
        };
        let mut session = Session::default();
        restore_prepared_continuation(
            &mut session,
            &continuation,
            &WorkflowConfig::default(),
            false,
            &[],
            Some(&factory),
        )
        .await
        .unwrap();
        assert!(Arc::ptr_eq(
            session.strict_history.as_ref().unwrap(),
            &fixture.history
        ));
        assert!(session.summary_usage.is_some());
        let factory: HistoryBackendFactory =
            Arc::new(|| panic!("installed backend must be retained"));
        restore_prepared_continuation(
            &mut session,
            &continuation,
            &WorkflowConfig::default(),
            false,
            &[],
            Some(&factory),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.summary.calls.load(Ordering::SeqCst),
            usize::from(mode == HistoryMode::Structured10)
        );
    }
}

#[tokio::test]
async fn r33_failed_backend_preparation_leaves_raw_workflow_and_window_unchanged() {
    let fixture = fixture(None, false);
    let before = fixture.saved.snapshot().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = calls.clone();
    let factory: HistoryBackendFactory = Arc::new(move || {
        factory_calls.fetch_add(1, Ordering::SeqCst);
        Err("fixture backend configuration missing".into())
    });
    let mut session = Session::default();
    session.push_user("current window");
    for factory in [None, Some(&factory)] {
        let error = restore_prepared_continuation(
            &mut session,
            &PreparedContinuation {
                kind: ContinuationKind::Resume,
                persistence: fixture.saved.clone(),
            },
            &WorkflowConfig::default(),
            false,
            &[],
            factory,
        )
        .await
        .unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(fixture.saved.snapshot().unwrap().state, before.state);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "current window");
        assert!(session.persistence.is_none());
        assert!(session.strict_history.is_none());
        assert!(session.summary_usage.is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.summary.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.embedder.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r33_publication_survives_invalid_override_and_is_reused_by_a_fresh_session() {
    let config = WorkflowConfig {
        always: vec!["user:r33-missing-required-skill".into()],
        ..WorkflowConfig::default()
    };
    let workflow = SessionWorkflow::new(config.clone());
    for saved_workflow in [None, Some(&workflow)] {
        let fixture = fixture(saved_workflow, false);
        assert!(
            fixture
                .history
                .recover_pending(&fixture.saved.store, &fixture.saved.database)
                .await
                .is_err()
        );
        fixture.embedder.fail.store(false, Ordering::SeqCst);
        let before = fixture.saved.snapshot().unwrap();
        let continuation = PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence: fixture.saved.clone(),
        };
        let mut session = Session {
            strict_history: Some(fixture.history.clone()),
            ..Session::default()
        };
        session.push_user("current window");
        let enabled = WorkflowConfig {
            enabled: true,
            ..config.clone()
        };
        let error =
            restore_prepared_continuation(&mut session, &continuation, &enabled, true, &[], None)
                .await
                .unwrap_err();
        assert!(error.contains("r33-missing-required-skill"), "{error}");
        let published = fixture.saved.snapshot().unwrap();
        assert_eq!(published.state.generation, before.state.generation + 1);
        assert_eq!(published.state.visible_structured_publication_ids.len(), 1);
        assert_eq!(
            published.state.workflow_config,
            before.state.workflow_config
        );
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "current window");
        assert!(session.persistence.is_none());
        // A fresh UI session retries through the same durable owner.
        let mut fresh = Session {
            strict_history: Some(fixture.history.clone()),
            ..Session::default()
        };
        restore_prepared_continuation(&mut fresh, &continuation, &config, false, &[], None)
            .await
            .unwrap();
        let final_state = fixture.saved.snapshot().unwrap().state;
        assert_eq!(
            final_state.generation,
            published.state.generation + u64::from(saved_workflow.is_none())
        );
        assert_eq!(
            final_state.visible_structured_publication_ids,
            published.state.visible_structured_publication_ids
        );
        assert_eq!(fresh.messages.len(), 11);
        assert_eq!(fixture.summary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.embedder.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn r33_picker_refuses_override_without_management_store() {
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};
    struct EnterOnce(bool);
    impl approver::KeyReader for EnterOnce {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            assert!(!self.0, "only one picker selection is expected");
            self.0 = true;
            Ok(KeyCode::Enter)
        }
    }
    let fixture = fixture(None, false);
    let root = fixture._root.path();
    let sessions = root.join("sessions");
    let project = root.join("project");
    let state_dir = root.join("state");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(project.join(".polaris")).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    #[cfg(unix)]
    let workspace = polaris_core::rewind_workspace::WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        polaris_core::rewind_workspace::RewindLimits::default(),
    )
    .unwrap();
    let project_id = persist::project_identity(&project).unwrap();
    let workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        ..WorkflowConfig::default()
    });
    let saved = PersistedSession::create(
        root,
        &state_dir.join("memory.sqlite3"),
        &project_id,
        HistoryMode::Strict10,
        Some(&workflow),
    )
    .unwrap();
    let mut seeded = Session::default();
    seeded.attach(saved).unwrap();
    seeded.push_user("picked saved turn");
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let backend = fixture.history.clone();
    let factory: HistoryBackendFactory = Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok((backend.clone(), polaris_provider::UsageMeter::default()))
    });
    let mut session = Session {
        disable_files_md_auto_regenerate: true,
        ..Session::default()
    };
    session.push_user("previous legacy window");
    let mut path = sessions.join("current.jsonl");
    let mut meta = path.with_extension("meta.json");
    let mut started = 1;
    let mut status = Status::Idle;
    let mut lines = vec![ratatui::text::Line::from("previous local output")];
    let terminal = RefCell::new(Terminal::new(TestBackend::new(80, 24)).unwrap());
    handle_resume(
        &terminal,
        &mut EnterOnce(false),
        &sessions,
        &state_dir,
        &project.display().to_string(),
        &WorkflowConfig::default(),
        true,
        &[],
        Some(&factory),
        #[cfg(unix)]
        Some(&workspace),
        #[cfg(unix)]
        None,
        &mut session,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
        &mut lines,
    )
    .await;
    assert!(
        matches!(&status, Status::Notice(message) if message.contains("管理操作の記録がありません"))
    );
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "previous legacy window");
    assert_eq!(path, sessions.join("current.jsonl"));
    assert_eq!(meta, path.with_extension("meta.json"));
    assert!(session.disable_files_md_auto_regenerate);
    assert!(session.summary_usage.is_none());
    assert_eq!(lines.len(), 1);
    assert!(session.workflow.is_none());
    assert!(
        seeded
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .workflow_config
            .unwrap()
            .config
            .enabled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.summary.calls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn f15_resume_confirmation_cancellation_is_byte_invariant_then_records_the_change() {
    use polaris_core::{
        management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1},
        rewind_workspace::{RewindLimits, WorkspaceOwner},
        workflow::WorkflowMode,
    };
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};

    struct Keys(VecDeque<KeyCode>);
    impl approver::KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            self.0
                .pop_front()
                .ok_or_else(|| std::io::Error::other("unexpected key read"))
        }
    }

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state_dir = root.path().join("state");
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(project.join(".git")).unwrap();
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(
        &state_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::create_dir(&sessions).unwrap();
    let workspace = WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let effects = {
        let lease = workspace.try_mutation().unwrap();
        ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap()
    };
    let project_id = persist::project_identity(&project).unwrap();
    let saved_workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        mode: WorkflowMode::Manual,
        ..WorkflowConfig::default()
    });
    let saved = PersistedSession::create(
        root.path(),
        &state_dir.join("memory.sqlite3"),
        &project_id,
        HistoryMode::Legacy,
        Some(&saved_workflow),
    )
    .unwrap();
    let saved_id = saved.snapshot().unwrap().state.session_id;
    let mut lock = saved.store.lock().unwrap();
    lock.append(Message::user("saved v2 request"), true)
        .unwrap();
    let snapshot = lock.snapshot().unwrap();
    lock.publish(&snapshot, snapshot.state.clone()).unwrap();
    let snapshot = lock.snapshot().unwrap();
    drop(lock);
    saved.accept_publication(&snapshot).unwrap();

    let requested = WorkflowConfig {
        enabled: true,
        mode: WorkflowMode::Adaptive,
        ..WorkflowConfig::default()
    };
    let journal = state_dir.join("management-effects-v1/journal.v1.jsonl");
    let before_state = saved.snapshot().unwrap().state;
    let before_journal = std::fs::read(&journal).unwrap();
    let mut session = Session::default();
    session.push_user("current window");
    let original_path = sessions.join("current.jsonl");
    let mut path = original_path.clone();
    let mut meta = path.with_extension("meta.json");
    let mut started = 1;
    let mut status = Status::Idle;
    let mut lines = vec![ratatui::text::Line::from("current local output")];
    let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 24)).unwrap());

    handle_resume(
        &terminal,
        &mut Keys(VecDeque::from([KeyCode::Enter, KeyCode::Esc])),
        &sessions,
        &state_dir,
        &project.display().to_string(),
        &requested,
        true,
        &[],
        None,
        Some(&workspace),
        Some(&effects),
        &mut session,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
        &mut lines,
    )
    .await;

    assert_eq!(saved.snapshot().unwrap().state, before_state);
    assert_eq!(std::fs::read(&journal).unwrap(), before_journal);
    assert_eq!(session.messages[0].content, "current window");
    assert_eq!(path, original_path);
    assert_eq!(effects.effects().unwrap().len(), 0);
    assert!(
        matches!(&status, Status::Notice(message) if message.contains("取り消")),
        "unexpected resume status: {}",
        match &status {
            Status::Notice(message) | Status::Error(message) => message.as_str(),
            _ => "no result notice",
        }
    );

    handle_resume(
        &terminal,
        &mut Keys(if cfg!(target_os = "linux") {
            // F15 approval and the separate D07 selection each need consent.
            VecDeque::from([KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('y')])
        } else {
            VecDeque::from([KeyCode::Enter, KeyCode::Char('y')])
        }),
        &sessions,
        &state_dir,
        &project.display().to_string(),
        &requested,
        true,
        &[],
        None,
        Some(&workspace),
        Some(&effects),
        &mut session,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
        &mut lines,
    )
    .await;

    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id,
        saved_id
    );
    assert_eq!(
        session.workflow.as_ref().unwrap().config.mode,
        WorkflowMode::Adaptive
    );
    assert_eq!(
        PersistedSession::open(
            root.path(),
            &state_dir.join("memory.sqlite3"),
            &project_id,
            &saved_id,
        )
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .workflow_config
        .unwrap()
        .config
        .mode,
        WorkflowMode::Adaptive
    );
    let effects = effects.effects().unwrap();
    assert_eq!(effects.len(), if cfg!(target_os = "linux") { 2 } else { 1 });
    let f15 = effects
        .iter()
        .find(|effect| effect.request.inventory.encoded() == "F15")
        .unwrap();
    assert!(matches!(
        f15.state,
        ManagementEffectStateV1::Completed { .. }
    ));
    if cfg!(target_os = "linux") {
        assert!(
            effects
                .iter()
                .any(|effect| effect.request.inventory.encoded() == "D07"
                    && matches!(effect.state, ManagementEffectStateV1::Completed { .. }))
        );
    }
}

#[cfg(unix)]
#[test]
fn unknown_f15_blocks_rewind_before_the_picker() {
    use polaris_core::{
        management_effect::{
            ManagementEffectHashV1, ManagementEffectScopeV1, ManagementEffectStoreV1,
            ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
            ManagementRewindDispositionV1, ManagementUnknownReasonV1,
            PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
        },
        rewind_workspace::{RewindLimits, WorkspaceOwner},
    };

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(
        &state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let workspace = WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let lease = workspace.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap();
    let request = PrepareManagementEffectV1 {
        operation_id: ManagementOperationIdV1::random().unwrap(),
        inventory: ManagementInventoryIdV1::parse("F15").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: effects.project_scope_hash(),
            session: Some(ManagementEffectHashV1::digest(b"saved-session")),
            generation: 7,
            revision: 3,
            permission: ManagementPermissionScopeV1::LocalSession,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(b"unknown-f15"),
        rewind: ManagementRewindDispositionV1::CapturedRestorable,
    };
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = effects
        .prepare(&lease, effects.cursor().unwrap(), request.clone())
        .unwrap()
    else {
        panic!("new F15 must prepare");
    };
    let started = effects.start(&lease, &prepared).unwrap();
    effects
        .mark_outcome_unknown(
            &lease,
            &started,
            ManagementUnknownReasonV1::InterruptedAfterStart,
        )
        .unwrap();

    let error = ensure_rewind_unblocked_with_lease(&project, &effects, &lease).unwrap_err();
    assert!(error.contains(&request.operation_id.to_string()));
    assert!(error.contains("F15"));
}

#[cfg(unix)]
#[test]
fn newly_unknown_f15_before_commit_rolls_back_the_rewind_transaction() {
    use polaris_core::{
        management_effect::{
            ManagementEffectHashV1, ManagementEffectScopeV1, ManagementEffectStoreV1,
            ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
            ManagementRewindDispositionV1, ManagementUnknownReasonV1,
            PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
        },
        rewind_workspace::{RewindLimits, WorkspaceOwner},
    };
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(
        &state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let file = project.join("task.txt");
    let owner = WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let policy =
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project)).unwrap();
    std::fs::write(&file, "before").unwrap();
    let pre = owner.capture(&policy).unwrap();
    std::fs::write(&file, "after").unwrap();
    let post = owner.capture(&policy).unwrap();
    let restored = owner
        .restore_for_branch(&post, &pre, "rewound-session")
        .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");

    let lease = owner.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap();
    let request = PrepareManagementEffectV1 {
        operation_id: ManagementOperationIdV1::random().unwrap(),
        inventory: ManagementInventoryIdV1::parse("F15").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: effects.project_scope_hash(),
            session: Some(ManagementEffectHashV1::digest(b"rewound-session")),
            generation: 8,
            revision: 4,
            permission: ManagementPermissionScopeV1::LocalSession,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(b"newly-unknown-f15"),
        rewind: ManagementRewindDispositionV1::CapturedRestorable,
    };
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = effects
        .prepare(&lease, effects.cursor().unwrap(), request.clone())
        .unwrap()
    else {
        panic!("new F15 must prepare");
    };
    let started = effects.start(&lease, &prepared).unwrap();
    effects
        .mark_outcome_unknown(
            &lease,
            &started,
            ManagementUnknownReasonV1::InterruptedAfterStart,
        )
        .unwrap();
    drop(lease);

    let failure = commit_rewind_if_unblocked(
        &owner,
        &effects,
        &restored.transaction_id,
        "rewound-session",
        &post,
        &pre,
    )
    .expect_err("a newly unknown F15 must prevent commit");
    assert!(!failure.fatal);
    assert!(failure.message.contains(&request.operation_id.to_string()));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
    assert!(matches!(
        owner.resolve_pending().unwrap(),
        polaris_core::rewind_workspace::ResolveOutcome::Clean
    ));
}
