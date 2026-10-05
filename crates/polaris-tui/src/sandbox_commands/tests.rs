//! TUI sandbox controls and startup permission-ceiling regression tests.

use super::*;
use polaris_core::{
    conversation_state::HistoryMode,
    management_effect::{ManagementDenialReasonV1, ManagementEffectStateV1},
    session_store::PersistedSession,
};
use polaris_sandbox::SandboxMode;
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use std::{collections::VecDeque, io};

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
    terminal: RefCell<Terminal<TestBackend>>,
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
        Self {
            _root: root,
            workspace,
            effects,
            session,
            terminal: RefCell::new(Terminal::new(TestBackend::new(140, 40)).unwrap()),
        }
    }
    fn call(
        &self,
        argv: &[&str],
        mode: SandboxMode,
        key: KeyCode,
        pending: bool,
    ) -> Result<Vec<Line<'static>>, String> {
        let roots = if mode == SandboxMode::WorkspaceWrite {
            vec![self.workspace.project_root().to_owned()]
        } else {
            vec![]
        };
        let policy = SandboxPolicy::new(mode, &roots).unwrap();
        handle(
            &serde_json::to_string(argv).unwrap(),
            &self.terminal,
            &mut Keys(VecDeque::from([key])),
            &self.session,
            &self.workspace,
            &self.effects,
            &policy,
            pending,
        )
    }
}

#[test]
fn m3e_c38_helper_executes_argv_and_enforces_readonly_ceiling() {
    let f = Fixture::new();
    let before = f
        .session
        .persistence
        .as_ref()
        .unwrap()
        .snapshot()
        .unwrap()
        .state;
    let lines = f
        .call(
            &["/usr/bin/printf", "sandbox-observed\u{001b}[31m"],
            SandboxMode::ReadOnly,
            KeyCode::Char('y'),
            false,
        )
        .unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains("sandbox-observed"))
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.to_string().contains('\u{001b}'))
    );
    assert!(matches!(
        f.effects.effects().unwrap()[0].state,
        ManagementEffectStateV1::Completed { .. }
    ));
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
    let target = f.workspace.project_root().join("write-refused");
    let script = format!("printf forbidden > '{}'", target.display());
    let lines = f
        .call(
            &["/bin/sh", "-c", &script],
            SandboxMode::ReadOnly,
            KeyCode::Char('y'),
            false,
        )
        .unwrap();
    assert!(!target.exists());
    assert!(!lines[0].to_string().contains("exit 0"));
}

#[test]
fn m3e_c38_helper_cancel_and_invalid_command_never_dispatch() {
    let f = Fixture::new();
    let target = f.workspace.project_root().join("cancelled");
    let script = format!("printf forbidden > '{}'", target.display());
    f.call(
        &["/bin/sh", "-c", &script],
        SandboxMode::WorkspaceWrite,
        KeyCode::Esc,
        false,
    )
    .unwrap();
    assert!(!target.exists());
    assert!(matches!(
        f.effects.effects().unwrap()[0].state,
        ManagementEffectStateV1::Denied {
            reason: ManagementDenialReasonV1::UserDeclined
        }
    ));
    let count = f.effects.effects().unwrap().len();
    assert!(
        f.call(
            &["printf", "invalid-relative"],
            SandboxMode::ReadOnly,
            KeyCode::Char('y'),
            false
        )
        .is_err()
    );
    assert!(
        f.call(
            &["/usr/bin/printf", "pending"],
            SandboxMode::ReadOnly,
            KeyCode::Char('y'),
            true
        )
        .is_err()
    );
    let lease = f.workspace.try_mutation().unwrap();
    assert!(
        f.call(
            &["/usr/bin/printf", "busy"],
            SandboxMode::ReadOnly,
            KeyCode::Char('y'),
            false
        )
        .is_err()
    );
    drop(lease);
    assert_eq!(f.effects.effects().unwrap().len(), count);
}
