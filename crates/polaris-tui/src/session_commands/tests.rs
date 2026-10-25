//! TUI session settings and history controls without provider requests.

use super::*;
use polaris_core::{
    conversation_memory::{
        EmbeddingModel, StrictEmbedder, StrictHistory, StrictSummaryProvider, SummaryRequest,
    },
    management_effect::ManagementEffectStateV1,
    session_store::PersistedSession,
};
use polaris_provider::{CompletionRequest, CompletionResponse, Message, ProviderError};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use std::{collections::VecDeque, future::Future, io, pin::Pin, sync::Arc};

struct CurrentProvider;
#[async_trait::async_trait]
impl Provider for CurrentProvider {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("settings never call a provider")
    }
    fn model(&self) -> Option<String> {
        Some("gpt-5.6-sol".into())
    }
    fn set_model(&self, _: &str) {
        panic!("root owns activation after commit")
    }
}

struct Keys(VecDeque<KeyCode>);
impl KeyReader for Keys {
    fn read_key(&mut self) -> io::Result<KeyCode> {
        self.0.pop_front().ok_or_else(|| io::Error::other("no key"))
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    workspace: WorkspaceOwner,
    effects: ManagementEffectStoreV1,
    session: Session,
    initial: SavedSessionSettingsV1,
    terminal: RefCell<Terminal<TestBackend>>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(
            &state,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let persisted = PersistedSession::create(
            root.path(),
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(persisted).unwrap();
        Self {
            _root: root,
            workspace,
            effects,
            session,
            initial: SavedSessionSettingsV1::new(
                "codex".into(),
                "gpt-5.6-sol".into(),
                "auto".into(),
            )
            .unwrap(),
            terminal: RefCell::new(Terminal::new(TestBackend::new(100, 24)).unwrap()),
        }
    }
    fn call(
        &mut self,
        request: &str,
        key: KeyCode,
        pending: bool,
        factory: Option<&HistoryBackendFactory>,
    ) -> Result<Vec<Line<'static>>, String> {
        self.call_result(request, key, pending, factory)
            .map(|result| result.lines)
    }
    fn call_result(
        &mut self,
        request: &str,
        key: KeyCode,
        pending: bool,
        factory: Option<&HistoryBackendFactory>,
    ) -> Result<CommandResult, String> {
        handle(
            request,
            &self.terminal,
            &mut Keys(VecDeque::from([key])),
            &mut self.session,
            &self.initial,
            &self.workspace,
            &self.effects,
            &CurrentProvider,
            factory,
            pending,
        )
    }
    fn snapshot(&self) -> ConversationSnapshot {
        self.session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
    }
    fn refuse(&mut self, request: &str) {
        let before = self.snapshot().state;
        assert!(
            self.call(request, KeyCode::Char('y'), false, None).is_err(),
            "{request}"
        );
        assert_eq!(self.snapshot().state, before, "{request}");
    }
}

#[test]
fn only_committed_settings_changes_request_binding_refresh() {
    let mut fixture = Fixture::new();
    let before = fixture.snapshot().state;
    for request in [
        "session",
        "session current",
        "session history",
        "goal",
        "history",
    ] {
        let result = fixture
            .call_result(request, KeyCode::Esc, true, None)
            .unwrap();
        assert!(!result.settings_changed, "{request}");
        assert!(!result.lines.is_empty(), "{request}");
    }
    let cancelled = fixture
        .call_result("memory configure on", KeyCode::Esc, false, None)
        .unwrap();
    assert!(!cancelled.settings_changed);
    assert_eq!(fixture.snapshot().state, before);
    let committed = fixture
        .call_result("memory configure on", KeyCode::Char('y'), false, None)
        .unwrap();
    assert!(committed.settings_changed);
    assert!(fixture.snapshot().state.session_settings.unwrap().remember);
    let goal = fixture
        .call_result("goal set finish", KeyCode::Char('y'), false, None)
        .unwrap();
    assert!(!goal.settings_changed);
    assert!(fixture.snapshot().state.goal.unwrap().active);
}

#[test]
fn m3c_helper_settings_goal_rows_save_normal_values_and_refuse_unique_inputs() {
    let mut fixture = Fixture::new();
    for request in [
        "memory configure on",
        "memory configure off",
        "tool-memory history",
        "tool-memory retrieval",
        "tool-memory off",
        "tool-memory endpoint http://localhost:8080/v1 embed-1",
        "tool-memory model embed-2",
        "compact threshold 1000",
        "session configure codex gpt-6-astra auto",
        "goal set 実装を完了する",
        "goal stop",
        "goal resume",
        "goal clear",
    ] {
        fixture
            .call(request, KeyCode::Char('y'), false, None)
            .unwrap();
    }
    let state = fixture.snapshot().state;
    let saved = state.session_settings.unwrap();
    assert!(!saved.remember);
    assert_eq!(saved.tool_memory, ToolMemoryModeV1::Off);
    assert_eq!(saved.tool_memory_embedding.unwrap().model, "embed-2");
    assert_eq!(saved.compact_threshold, Some(1000));
    assert_eq!(saved.model, "gpt-6-astra");
    assert_eq!(CurrentProvider.model().as_deref(), Some("gpt-5.6-sol"));
    assert!(!state.goal.unwrap().active);
    for request in [
        "memory configure unsupported",
        "tool-memory external",
        "tool-memory endpoint https://example.com/v1 model",
        "tool-memory model",
        "compact threshold 999",
        "session configure other-provider gpt-6-astra auto",
        "session configure codex unavailable-model auto",
        "session configure codex gpt-6-astra high",
        "goal set",
    ] {
        fixture.refuse(request);
    }
    let before = fixture.snapshot().state;
    assert!(
        fixture
            .call("memory configure on", KeyCode::Char('y'), true, None)
            .is_err()
    );
    assert_eq!(fixture.snapshot().state, before);
    fixture
        .call("tool-memory endpoint off", KeyCode::Char('y'), false, None)
        .unwrap();
    fixture.refuse("tool-memory model embed-3");
    assert!(
        fixture
            .effects
            .effects()
            .unwrap()
            .iter()
            .all(|effect| matches!(effect.state, ManagementEffectStateV1::Completed { .. }))
    );
}

#[test]
fn m3c_helper_cancel_records_denial_and_persistence_error_refuses_changes() {
    let mut fixture = Fixture::new();
    let before = fixture.snapshot().state;
    let lines = fixture
        .call("memory configure on", KeyCode::Esc, false, None)
        .unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains("cancelled"))
    );
    assert_eq!(fixture.snapshot().state, before);
    assert!(matches!(
        fixture.effects.effects().unwrap()[0].state,
        ManagementEffectStateV1::Denied { .. }
    ));
    fixture.session.persistence_error = Some("saved append failure".into());
    fixture.refuse("memory configure on");
}

type StrictFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;
struct NoInference {
    metadata: EmbeddingModel,
}
impl StrictSummaryProvider for NoInference {
    fn summarize<'a>(&'a self, _: SummaryRequest) -> StrictFuture<'a, String> {
        panic!("history initialization never summarizes")
    }
}
impl StrictEmbedder for NoInference {
    fn metadata(&self) -> &EmbeddingModel {
        &self.metadata
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
        panic!("history initialization never embeds")
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        panic!("history initialization never retrieves")
    }
}

#[test]
fn m3c_helper_history_requires_backend_and_initializes_only_empty_conversations() {
    let mut fixture = Fixture::new();
    fixture.refuse("session history strict10");
    assert!(fixture.session.strict_history.is_none());
    let backend = Arc::new(NoInference {
        metadata: EmbeddingModel {
            model: "fixture".into(),
            revision: "v1".into(),
            dimension: 2,
        },
    });
    let history = Arc::new(StrictHistory::new(backend.clone(), backend));
    let factory: HistoryBackendFactory =
        Arc::new(move || Ok((history.clone(), Default::default())));
    let before = fixture.snapshot().state;
    fixture
        .call(
            "session history strict10",
            KeyCode::Esc,
            false,
            Some(&factory),
        )
        .unwrap();
    assert_eq!(fixture.snapshot().state, before);
    assert!(fixture.session.strict_history.is_none());
    fixture
        .call(
            "session history strict10",
            KeyCode::Char('y'),
            false,
            Some(&factory),
        )
        .unwrap();
    assert_eq!(fixture.snapshot().state.history_mode, HistoryMode::Strict10);
    assert!(fixture.session.strict_history.is_some());
    fixture
        .call(
            "memory configure structured10",
            KeyCode::Char('y'),
            false,
            Some(&factory),
        )
        .unwrap();
    assert_eq!(
        fixture.snapshot().state.history_mode,
        HistoryMode::Structured10
    );
    fixture
        .session
        .persistence
        .as_ref()
        .unwrap()
        .append(Message::user("actual history"), true, None)
        .unwrap();
    fixture.refuse("session history legacy");
}

#[test]
fn m3c_helper_read_views_bound_history_and_report_range_or_size_errors() {
    let mut fixture = Fixture::new();
    let saved = fixture.session.persistence.as_ref().unwrap().clone();
    for n in 0..21 {
        saved
            .append(Message::user(format!("message-{n}")), true, None)
            .unwrap();
    }
    let lines = fixture.call("history 0", KeyCode::Esc, true, None).unwrap();
    assert_eq!(lines.len(), HISTORY_PAGE_MESSAGES + 1);
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains("message-19"))
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.to_string().contains("message-20"))
    );
    let tail = fixture
        .call("history 20", KeyCode::Esc, true, None)
        .unwrap();
    assert!(
        tail.iter()
            .any(|line| line.to_string().contains("message-20"))
    );
    assert!(
        fixture
            .call("history 22", KeyCode::Esc, true, None)
            .is_err()
    );
    assert!(
        fixture
            .call("session current", KeyCode::Esc, true, None)
            .unwrap()
            .iter()
            .any(|line| line.to_string().contains("gpt-5.6-sol"))
    );
    assert!(
        fixture
            .call("goal current", KeyCode::Esc, true, None)
            .unwrap()[0]
            .to_string()
            .contains("No saved goal")
    );
    saved
        .append(Message::user("x".repeat(MAX_VIEW_BYTES + 1)), true, None)
        .unwrap();
    assert!(
        fixture
            .call("history 21", KeyCode::Esc, true, None)
            .is_err()
    );
    assert!(fixture.effects.effects().unwrap().is_empty());
}
