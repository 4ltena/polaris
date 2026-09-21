//! Request-boundary regressions for source-bound, host-only input preparation.

use super::{AgentOutcome, ToolContext, run, run_loop};
use crate::{
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    conversation_state::HistoryMode,
    input_preparation::{InputPreparationMode, prepare},
    session::Session,
    session_store::PersistedSession,
    stop::StopTracker,
};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, Role, ToolCall,
};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

const ENCODING_PREFIX: &str = "[Polaris input encoding v1]\n";

struct Scripted {
    replies: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl Scripted {
    fn new(replies: impl IntoIterator<Item = CompletionResponse>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra provider request"))
    }
}

struct UnusedPool;

#[async_trait::async_trait]
impl Provider for UnusedPool {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("input preparation must not use the child provider pool")
    }
}

struct NoApproval;

impl Approver for NoApproval {
    fn ask(&mut self, _: &str) -> Decision {
        panic!("these request-only fixtures must not ask for approval")
    }
}

fn answer(text: &str) -> CompletionResponse {
    CompletionResponse {
        text: text.into(),
        ..Default::default()
    }
}

fn long_input() -> String {
    let mut paragraph = String::new();
    while crate::budget::count_tokens(&paragraph) < 700 {
        paragraph.push_str("The background remains the same; preserve the names and conditions. ");
    }
    paragraph.push_str("\n\n");
    paragraph.repeat(9)
}

fn persisted_session(root: &Path) -> (Session, PersistedSession) {
    let saved = PersistedSession::create(
        &root.join("sessions"),
        &root.join("index.sqlite3"),
        "project",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let mut session = Session::new();
    session.attach(saved.clone()).unwrap();
    session.input_preparation = InputPreparationMode::RepeatRefs;
    session.disable_files_md_auto_regenerate = true;
    (session, saved)
}

async fn root_run(provider: &Scripted, session: &mut Session, log: &Path) -> AgentOutcome {
    let always_on = crate::prompt::assemble_always_on("", "", &[]);
    let sandbox =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut approver = NoApproval;
    let mut context = ToolContext {
        sandbox: &sandbox,
        helper: Path::new("/unused-input-preparation-helper"),
        gate: &mut gate,
        approver: &mut approver,
    };
    run(
        provider,
        session,
        Arc::new(tokio::sync::Mutex::new(AuditLog::open(log).unwrap())),
        &mut StopTracker::new(6),
        &always_on,
        &[],
        &[],
        Arc::new(UnusedPool),
        1,
        1,
        None,
        &mut context,
    )
    .await
    .unwrap()
}

fn audit_rows(log: &Path, tool: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|row| row["tool"] == tool)
        .collect()
}

fn assert_counted_requests(log: &Path, requests: &[CompletionRequest]) {
    let rows = audit_rows(log, "workflow_context");
    assert_eq!(rows.len(), requests.len());
    for (row, request) in rows.iter().zip(requests) {
        let recorded: serde_json::Value =
            serde_json::from_str(row["result"].as_str().unwrap()).unwrap();
        let sent = serde_json::to_value(crate::budget::request_context_tokens(request)).unwrap();
        assert_eq!(
            recorded, sent,
            "audit must count the exact provider request"
        );
    }
}

fn assert_saved_input(saved: &PersistedSession, original: &str) {
    let snapshot = saved.snapshot().unwrap();
    let latest = snapshot
        .events
        .iter()
        .rev()
        .find(|event| event.starts_turn)
        .unwrap();
    assert_eq!(latest.message.content.as_bytes(), original.as_bytes());
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| !event.message.content.starts_with(ENCODING_PREFIX))
    );
}

#[tokio::test]
async fn fresh_root_projects_the_sent_copy_and_uses_it_for_the_compaction_decision() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("audit.jsonl");
    let (mut session, saved) = persisted_session(dir.path());
    session.push_user("old task");
    session.push_assistant(&"old detail ".repeat(800), vec![]);
    session.push_user("recent task");
    session.push_assistant("recent answer", vec![]);
    let original = long_input();
    session.push_user(&original);
    session.check_persistence().unwrap();
    let projection = prepare(&original, InputPreparationMode::RepeatRefs)
        .projection
        .unwrap();
    let always_on = crate::prompt::assemble_always_on("", "", &[]);
    let raw_tokens = crate::budget::always_on_tokens(always_on.system(), always_on.tools())
        + crate::compaction::session_tokens(&session.messages);
    // The raw form crosses this threshold; the actual projected form does not.
    session.compaction_threshold =
        Some(raw_tokens - (projection.input_tokens - projection.output_tokens) / 2);
    let provider = Scripted::new([answer("done")]);
    assert_eq!(root_run(&provider, &mut session, &log).await.text, "done");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "no preparation or unnecessary summary model call"
    );
    let sent = &requests[0].messages[4];
    assert_eq!(sent.role, Role::User);
    assert!(sent.content.starts_with(ENCODING_PREFIX));
    assert_eq!(sent.content, projection.encoded);
    assert_eq!(session.messages[4].content, original);
    assert_saved_input(&saved, &original);
    assert_counted_requests(&log, &requests);
    let observations = audit_rows(&log, "input-preparation");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["detail"], "candidate_applied");
    assert_eq!(
        audit_rows(&log, "input-preparation-request")[0]["detail"],
        "applied"
    );
    let metadata: serde_json::Value =
        serde_json::from_str(observations[0]["result"].as_str().unwrap()).unwrap();
    assert_eq!(metadata["output_tokens"], projection.output_tokens);
    assert!(!std::fs::read_to_string(&log).unwrap().contains(&original));
}

#[tokio::test]
async fn a_tool_continuation_reuses_one_projection_and_keeps_durable_raw() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("audit.jsonl");
    let (mut session, saved) = persisted_session(dir.path());
    let original = long_input();
    session.push_user(&original);
    let provider = Scripted::new([
        CompletionResponse {
            tool_calls: vec![ToolCall {
                id: "skill-call".into(),
                name: "skill".into(),
                arguments: serde_json::json!({"name":"not-installed"}),
            }],
            ..Default::default()
        },
        answer("done"),
    ]);
    root_run(&provider, &mut session, &log).await;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "only the two scripted main requests run");
    assert!(requests[0].messages[0].content.starts_with(ENCODING_PREFIX));
    assert_eq!(
        requests[0].messages[0].content,
        requests[1].messages[0].content
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some("skill-call"))
    );
    assert_eq!(audit_rows(&log, "input-preparation").len(), 1);
    assert_eq!(session.messages[0].content, original);
    assert_saved_input(&saved, &original);
    assert_counted_requests(&log, &requests);
}

#[tokio::test]
async fn reentering_or_resuming_an_old_source_does_not_prepare_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("audit.jsonl");
    let (mut session, saved) = persisted_session(dir.path());
    let original = long_input();
    session.push_user(&original);
    let provider = Scripted::new([answer("first"), answer("second"), answer("resumed")]);
    root_run(&provider, &mut session, &log).await;
    root_run(&provider, &mut session, &log).await;
    let mut resumed = Session::new();
    resumed.input_preparation = InputPreparationMode::RepeatRefs;
    resumed.disable_files_md_auto_regenerate = true;
    resumed.attach(saved.clone()).unwrap();
    root_run(&provider, &mut resumed, &log).await;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].messages[0].content.starts_with(ENCODING_PREFIX));
    assert_eq!(requests[1].messages[0].content, original);
    assert_eq!(requests[2].messages[0].content, original);
    assert_eq!(audit_rows(&log, "input-preparation").len(), 1);
    assert_saved_input(&saved, &original);
    assert_counted_requests(&log, &requests);
}

#[tokio::test]
async fn a_child_cannot_project_even_a_fresh_persisted_eligible_input() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("audit.jsonl");
    let (mut session, saved) = persisted_session(dir.path());
    let original = long_input();
    session.push_user(&original);
    assert!(session.input_source.is_some());
    let provider = Scripted::new([answer("child done")]);
    let always_on = crate::prompt::assemble_always_on("", "", &[]);
    let sandbox =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut approver = NoApproval;
    let mut context = ToolContext {
        sandbox: &sandbox,
        helper: Path::new("/unused-input-preparation-helper"),
        gate: &mut gate,
        approver: &mut approver,
    };
    run_loop(
        &provider,
        &mut session,
        Arc::new(tokio::sync::Mutex::new(AuditLog::open(&log).unwrap())),
        &mut StopTracker::new(3),
        always_on.system(),
        always_on.tools(),
        &[],
        &[],
        Arc::new(UnusedPool),
        1,
        1,
        "child",
        "",
        false,
        None,
        &mut context,
        None,
    )
    .await
    .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].content, original);
    assert!(audit_rows(&log, "input-preparation").is_empty());
    assert_saved_input(&saved, &original);
    assert_counted_requests(&log, &requests);
}

#[tokio::test]
async fn successful_compaction_invalidates_the_slot_and_sends_original_current_input() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("audit.jsonl");
    let (mut session, saved) = persisted_session(dir.path());
    session.push_user("old task");
    session.push_assistant(&"old detail ".repeat(800), vec![]);
    session.push_user("recent task");
    session.push_assistant("recent answer", vec![]);
    let original = long_input();
    session.push_user(&original);
    session.compaction_threshold = Some(1);
    let provider = Scripted::new([answer("compact summary"), answer("done")]);
    assert_eq!(root_run(&provider, &mut session, &log).await.text, "done");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one existing summary and one main request only"
    );
    assert!(requests[0].system.contains("summarizing"));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == original)
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|message| !message.content.starts_with(ENCODING_PREFIX))
    );
    assert!(session.input_source.is_none());
    assert_eq!(
        audit_rows(&log, "input-preparation-request")[0]["detail"],
        "source_invalidated"
    );
    assert_saved_input(&saved, &original);
    // The separate summary request is not an agent request_context audit entry.
    assert_counted_requests(&log, &requests[1..]);
}
