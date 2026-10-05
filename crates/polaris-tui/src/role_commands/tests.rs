//! TUI role and tier controls with fixed-provider and permission checks.

use super::*;
use polaris_core::{
    conversation_state::HistoryMode,
    management_effect::{ManagementDenialReasonV1, ManagementEffectStateV1},
    session_store::PersistedSession,
};
use polaris_provider::{CompletionRequest, CompletionResponse, ProviderError};
use polaris_skills::AgentAccess;
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use std::{collections::VecDeque, io};

struct Fixed(&'static str);
#[async_trait::async_trait]
impl Provider for Fixed {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("role controls do not infer")
    }
    fn model(&self) -> Option<String> {
        Some(self.0.into())
    }
    fn set_model(&self, _: &str) {
        panic!("fixed tier must not be retuned")
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
    provider: Arc<dyn Provider>,
    catalog: Vec<AgentType>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state).unwrap();
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
        let tier: Arc<dyn Provider> = Arc::new(Fixed("fixed-tier-model"));
        let provider: Arc<dyn Provider> = Arc::new(RoleProvider::tiers_only(
            Arc::new(Fixed("parent-model")),
            HashMap::from([("subgiant".into(), tier)]),
        ));
        let catalog = vec![AgentType {
            name: "worker".into(),
            description: "test".into(),
            body: String::new(),
            path: project.clone(),
            allowed_tools: vec![],
            access: AgentAccess::Read,
            tier: Tier::Subgiant,
            tier_min: Tier::Dwarf,
            wall_seconds: 30,
            max_turns: 1,
            workflow_phase: None,
            continuation: false,
            output_schema: project.join("schema.json"),
        }];
        Self {
            _root: root,
            workspace,
            effects,
            session,
            initial: SavedSessionSettingsV1::new(
                "fixed".into(),
                "parent-model".into(),
                "auto".into(),
            )
            .unwrap(),
            terminal: RefCell::new(Terminal::new(TestBackend::new(120, 30)).unwrap()),
            provider,
            catalog,
        }
    }
    fn call(
        &self,
        request: &str,
        key: KeyCode,
        pending: bool,
    ) -> Result<Vec<Line<'static>>, String> {
        handle(
            request,
            &self.terminal,
            &mut Keys(VecDeque::from([key])),
            &self.session,
            &self.initial,
            &self.workspace,
            &self.effects,
            self.provider.as_ref(),
            &self.catalog,
            pending,
        )
    }
    fn saved(&self) -> SavedSessionSettingsV1 {
        self.session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_settings
            .unwrap()
    }
}

#[test]
fn m3e_d13_helper_binds_actual_fixed_tier_and_refuses_missing_bindings() {
    let f = Fixture::new();
    assert!(!f.initial.role_bindings_active);
    let unchanged = activation(f.provider.clone(), &f.initial, &[]).unwrap();
    assert!(Arc::ptr_eq(&unchanged, &f.provider));
    assert!(unchanged.resolve_role("worker").unwrap().is_none());
    assert!(
        f.call("roles current", KeyCode::Esc, false).unwrap()[0]
            .to_string()
            .contains("無効")
    );
    f.call("roles bind worker subgiant", KeyCode::Char('y'), false)
        .unwrap();
    let saved = f.saved();
    assert!(saved.role_bindings_active);
    assert_eq!(
        saved.role_bindings.get("worker").map(String::as_str),
        Some("subgiant")
    );
    let active = activation(f.provider.clone(), &saved, &f.catalog).unwrap();
    let routed = active.resolve_role("worker").unwrap().unwrap();
    assert_eq!(routed.model().as_deref(), Some("fixed-tier-model"));
    assert!(Arc::ptr_eq(
        &routed,
        &f.provider.resolve_tier("subgiant").unwrap().unwrap()
    ));
    assert!(active.resolve_role("missing-role").is_err());
    assert!(active.resolve_tier("subgiant").unwrap().is_some());
    let before = f.saved();
    let count = f.effects.effects().unwrap().len();
    for command in [
        "roles bind absent subgiant",
        "roles bind worker giant",
        "roles bind worker unavailable",
    ] {
        assert!(f.call(command, KeyCode::Char('y'), false).is_err());
        assert_eq!(f.saved(), before);
    }
    assert_eq!(f.effects.effects().unwrap().len(), count);
    f.call("roles unbind worker", KeyCode::Char('y'), false)
        .unwrap();
    assert!(f.saved().role_bindings.is_empty());
    assert!(f.saved().role_bindings_active);
    assert!(
        activation(f.provider.clone(), &f.saved(), &f.catalog)
            .unwrap()
            .resolve_role("worker")
            .is_err()
    );
    assert!(
        f.effects
            .effects()
            .unwrap()
            .iter()
            .all(|effect| matches!(effect.state, ManagementEffectStateV1::Completed { .. }))
    );
}

#[test]
fn m3e_d13_helper_cancel_clear_and_pending_preserve_exact_owner_state() {
    let f = Fixture::new();
    let before = f
        .session
        .persistence
        .as_ref()
        .unwrap()
        .snapshot()
        .unwrap()
        .state;
    f.call("roles bind worker subgiant", KeyCode::Esc, false)
        .unwrap();
    assert_eq!(
        f.session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state,
        before
    );
    assert!(matches!(
        f.effects.effects().unwrap()[0].state,
        ManagementEffectStateV1::Denied {
            reason: ManagementDenialReasonV1::UserDeclined
        }
    ));
    assert!(
        f.call("roles bind worker subgiant", KeyCode::Char('y'), true)
            .is_err()
    );
    f.call("roles bind worker subgiant", KeyCode::Char('y'), false)
        .unwrap();
    f.call("roles clear", KeyCode::Char('y'), false).unwrap();
    assert!(f.saved().role_bindings.is_empty());
    assert!(
        f.call("roles unbind worker", KeyCode::Char('y'), false)
            .is_err()
    );
}
