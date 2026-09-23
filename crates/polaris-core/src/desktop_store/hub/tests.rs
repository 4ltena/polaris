//! Pure projection and difference; no store and no I/O.
use super::*;
use polaris_desktop_protocol::ids::*;
use polaris_desktop_protocol::snapshot::{
    Acceptance, AcceptanceState, ApprovalDisplay, BlockerKind, Configuration, Draft,
    PendingApproval, Run, Task, TaskBlocker,
};

fn configuration() -> Configuration {
    Configuration {
        configuration_revision: DecimalU64::new(1),
        provider: "codex".into(),
        model: "gpt-6-astra".into(),
        effort: "medium".into(),
        history_mode: Default::default(),
    }
}

fn empty() -> Sidecar {
    Sidecar {
        inherited_turn_count: DecimalU64::new(0),
        conversation_memory: None,
        session_revision: DecimalU64::new(0),
        draft: Draft {
            draft_revision: DecimalU64::new(0),
            text: String::new(),
            attachment_ids: Vec::new(),
        },
        configuration: configuration(),
        role_bindings: None,
        tier_bindings: None,
        goal: None,
        instructions: Vec::new(),
        policy_revision: DecimalU64::new(1),
        tasks: Vec::new(),
        children: Vec::new(),
        runs: Vec::new(),
        requests: Vec::new(),
        source_applies: Vec::new(),
        unresolved_approvals: Vec::new(),
        approval_records: Vec::new(),
        workflow: None,
        workflow_artifacts: Default::default(),
    }
}

fn sidecar() -> Sidecar {
    let mut state = empty();
    state.tasks = vec![Task {
        task_id: TaskId::new("t1").unwrap(),
        title: "最初の作業".into(),
        state: TaskState::Implementing,
        acceptance: vec![
            Acceptance {
                criterion: "検査が通る".into(),
                state: AcceptanceState::Pending,
                evidence: String::new(),
            },
            Acceptance {
                criterion: "記録した".into(),
                state: AcceptanceState::Passed,
                evidence: "checks/x.json".into(),
            },
        ],
        blockers: vec![],
    }];
    state
}

fn run(id: &str, state: RunState) -> crate::desktop_store::RunRecord {
    let mut record = crate::desktop_store::RunRecord {
        run: Run {
            run_id: RunId::new(id).unwrap(),
            attempt_id: AttemptId::new(format!("a{id}")).unwrap(),
            state,
            task_ids: vec![TaskId::new("t1").unwrap()],
        },
        admission: None,
        configuration: configuration(),
        role_bindings: None,
        tier_bindings: None,
        account: None,
        policy_revision: DecimalU64::new(1),
        input: Draft {
            draft_revision: DecimalU64::new(1),
            text: "入力".into(),
            attachment_ids: Vec::new(),
        },
        mcp_prompt: None,
        operations: Vec::new(),
        result_id: None,
        usage: None,
        memory: None,
        memory_resources: None,
        workflow: None,
        history_gap: None,
    };
    record.account = crate::desktop_store::SavedAccount::new("reg-a".into(), 1).ok();
    record
}

#[test]
fn the_window_projects_what_is_already_saved_without_copying_originals() {
    let mut state = sidecar();
    state.runs = vec![run("r1", RunState::Running)];
    state.unresolved_approvals = vec![PendingApproval {
        approval_id: ApprovalId::new("ap1").unwrap(),
        run_id: RunId::new("r1").unwrap(),
        attempt_id: AttemptId::new("ar1").unwrap(),
        operation_id: OperationId::new("op1").unwrap(),
        operation: "bash".into(),
        scope: "workspace".into(),
        payload_hash: "a".repeat(64),
        policy_revision: DecimalU64::new(1),
        expires_at_unix_ms: DecimalU64::new(1_700_000_000_000),
        state: PendingApprovalState::Pending,
        display: ApprovalDisplay {
            title: "コマンドの実行".into(),
            description: "printf".into(),
            choices: vec![],
        },
    }];
    let view = view(&state, 7, None);
    assert_eq!(view.revision, 7);
    assert_eq!(view.tasks[0].unmet_acceptance, 1);
    assert_eq!(view.runs[0].account.as_deref(), Some("reg-a"));
    assert_eq!(view.approvals[0].operation, "bash");
    // The window carries references and states, never the input or a hash.
    let json = serde_json::to_string(&view).unwrap();
    assert!(!json.contains("payload_hash") && !json.contains(&"a".repeat(64)));
    assert!(!json.contains("入力"));
}

#[test]
fn a_caller_whose_base_is_unknown_or_old_is_resynchronized() {
    let state = sidecar();
    let first = view(&state, 1, None);
    assert!(matches!(delta(None, &first, None), Delta::Resync(_)));
    assert!(matches!(delta(None, &first, Some(1)), Delta::Resync(_)));
    assert!(matches!(
        delta(Some(&first), &first, Some(2)),
        Delta::Resync(_)
    ));
    let Delta::Changes(changes) = delta(Some(&first), &first, Some(1)) else {
        panic!("a matching base produces changes");
    };
    assert!(changes.is_empty());
    assert_eq!((changes.base, changes.revision), (1, 1));
}

#[test]
fn only_what_changed_is_returned_and_a_gone_item_is_named() {
    let mut state = sidecar();
    state.runs = vec![run("r1", RunState::Running)];
    let first = view(&state, 1, None);

    state.runs[0].run.state = RunState::Succeeded;
    state.runs.push(run("r2", RunState::Queued));
    state.tasks[0].blockers = vec![TaskBlocker {
        run_id: RunId::new("r1").unwrap(),
        attempt_id: AttemptId::new("ar1").unwrap(),
        kind: BlockerKind::OutcomeUnknown,
        detail: "承認待ち".into(),
    }];
    let second = view(&state, 2, None);
    let Delta::Changes(changes) = delta(Some(&first), &second, Some(1)) else {
        panic!("changes");
    };
    assert_eq!(changes.runs.changed.len(), 2);
    assert!(changes.runs.removed.is_empty());
    assert_eq!(changes.tasks.changed[0].blockers, vec!["outcomeunknown"]);
    assert!(changes.approvals.is_empty());
    assert!(changes.phase.is_none());

    // A finished run leaves the window; its key is reported as removed.
    state.runs.remove(0);
    let third = view(&state, 3, None);
    let Delta::Changes(changes) = delta(Some(&second), &third, Some(2)) else {
        panic!("changes");
    };
    assert_eq!(changes.runs.removed, vec!["r1:ar1".to_string()]);
    assert!(changes.runs.changed.is_empty());
    assert!(changes.tasks.is_empty());
}
