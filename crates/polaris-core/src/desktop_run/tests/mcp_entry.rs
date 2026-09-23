//! Real worker/owner boundary with synthetic providers and a loopback MCP server.
use super::*;
use crate::{
    approval::ApprovalPolicy,
    desktop_store::{PrototypeRoot, SavedWorkflow, Writer},
    execution_owner::RunExecution,
    mcp::{operation::ExternalCeiling, runtime::RuntimeConfig},
};
use polaris_desktop_protocol::{ids::*, request::McpPromptChoice, run_state::Observation};
use polaris_mcp::{
    registration::{Registry, Server},
    store::Secrets,
};
use polaris_provider::{Message, Role};
use std::{collections::BTreeMap, sync::atomic::AtomicBool};

mod agent_dispatch;

fn choice() -> McpPromptChoice {
    McpPromptChoice {
        server: "fixture".into(),
        prompt: "prompt".into(),
        arguments: Default::default(),
    }
}

fn attach(
    input: &mut DesktopRunInput,
    servers: Vec<Server>,
    workflow: Option<SavedWorkflow>,
) -> (PrototypeRoot, Writer, RunExecution) {
    let (root, writer, target) = history::writer_with_workflow(
        input.target.run_id.clone(),
        "fixture input",
        Some(choice()),
        workflow,
    );
    input.target = target.clone();
    let mut owner = RunExecution::new(&target, Arc::new(AtomicBool::new(false)));
    let port = owner
        .configure_mcp(
            &target,
            SessionId::new("desktop-session").unwrap(),
            ExternalCeiling::Denied,
            ApprovalPolicy::Never,
        )
        .unwrap();
    input.request_context = DesktopRequestContext::Mcp {
        config: Box::new(
            RuntimeConfig::new(
                Registry {
                    schema_version: 1,
                    revision: 1,
                    servers,
                },
                "project".into(),
                Arc::new(Secrets::new(1, BTreeMap::new()).unwrap()),
                BTreeMap::new(),
            )
            .unwrap(),
        ),
        port,
        prompt: Some(choice()),
        preselection: Default::default(),
    };
    (root, writer, owner)
}

async fn drain(
    worker: &mut DesktopRun,
    owner: &mut RunExecution,
    writer: &mut Writer,
    target: &RunTarget,
) -> DesktopRunCompletion {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            owner.step(writer, target).unwrap();
            if let Some(result) = worker.try_join() {
                assert!(owner.quiescent());
                return result.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("worker did not join and drain")
}

struct Inspect {
    calls: Arc<AtomicUsize>,
    behavior: Behavior,
    with_tool: bool,
}
#[async_trait::async_trait]
impl Provider for Inspect {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.messages[0].role, Role::User);
        assert!(request.messages[0].content.contains("fixture input"));
        assert!(request.messages[0].content.contains("selected prompt"));
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .count(),
            1
        );
        if self.with_tool {
            // The supplemental words must not select optional skill metadata.
            assert!(request.system.contains("fixture-query"));
            assert!(!request.system.contains("supplement-query"));
        }
        match self.behavior {
            Behavior::Panic => panic!("synthetic provider panic after prompt ACK"),
            Behavior::Error => Err(polaris_provider::ProviderError::Http("fixture".into())),
            Behavior::Pending => std::future::pending().await,
            Behavior::Complete => Ok(if self.with_tool && index == 0 {
                polaris_provider::CompletionResponse {
                    tool_calls: vec![polaris_provider::ToolCall {
                        id: "optional-skill".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({"q":"unmatched"}),
                    }],
                    ..Default::default()
                }
            } else {
                polaris_provider::CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                }
            }),
        }
    }
}

#[tokio::test]
async fn prompt_ack_preserves_raw_and_query_and_checkpoint_finish_use_only_turn_suffix() {
    let (server, methods, server_task) = mcp_server::server_fixture("").await;
    let (_temp, mut input, mut requests, calls) = fixture(Behavior::Complete);
    let previous = SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
        crate::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        },
    ));
    let (_root, mut writer, mut owner) = attach(&mut input, vec![server], Some(previous.clone()));
    let target = input.target.clone();
    for (name, description) in [
        ("fixture-query", "fixture input"),
        ("supplement-query", "selected prompt"),
    ] {
        input.skills.push(polaris_skills::Skill {
            name: name.into(),
            description: description.into(),
            body: "irrelevant".into(),
            path: format!("/not-opened/{name}/SKILL.md").into(),
        });
    }
    input.provider = Arc::new(Inspect {
        calls: calls.clone(),
        behavior: Behavior::Complete,
        with_tool: true,
    });
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = drain(&mut worker, &mut owner, &mut writer, &target).await;
    assert!(completion.result.is_ok(), "{:?}", completion.result.err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(requests.try_recv().is_err());
    assert_eq!(
        completion
            .turn_suffix
            .iter()
            .map(|m| m.role)
            .collect::<Vec<_>>(),
        vec![Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(completion.session.messages.len(), 5);
    assert_eq!(completion.session.messages[0].content, "fixture input");
    assert!(
        completion.session.messages[1]
            .content
            .contains("selected prompt")
    );
    assert_eq!(writer.snapshot().unwrap().raw.len(), 2);
    let mut candidate = previous.clone();
    candidate.state.phase = crate::workflow::Phase::Review;
    candidate.state.revision += 1;
    // This is the old service extraction: it wrongly includes the supplement.
    assert!(
        writer
            .checkpoint_workflow(
                &target,
                completion.session.messages[1..].to_vec(),
                previous.clone(),
                candidate.clone(),
                None,
                None,
            )
            .is_err()
    );
    writer
        .checkpoint_workflow(
            &target,
            completion.turn_suffix.clone(),
            previous,
            candidate.clone(),
            None,
            None,
        )
        .unwrap();
    assert_eq!(writer.snapshot().unwrap().raw.len(), 5);
    writer
        .finish_with_messages_and_workflow(
            &target,
            &OperationId::new("main-fixture-run").unwrap(),
            Observation::Succeeded,
            ResultId::new("final").unwrap(),
            completion.turn_suffix,
            Some(candidate),
        )
        .unwrap();
    let raw: Vec<Message> = writer
        .snapshot()
        .unwrap()
        .raw
        .into_iter()
        .map(|event| event.message)
        .collect();
    assert_eq!(
        serde_json::to_value(raw).unwrap(),
        serde_json::to_value(completion.session.messages).unwrap()
    );
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "prompts/get")
            .count(),
        1
    );
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn provider_failure_or_panic_after_prompt_ack_joins_without_resending_or_losing_raw() {
    for behavior in [Behavior::Error, Behavior::Panic] {
        let (server, methods, server_task) = mcp_server::server_fixture("").await;
        let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
        let (_root, mut writer, mut owner) = attach(&mut input, vec![server], None);
        let target = input.target.clone();
        input.provider = Arc::new(Inspect {
            calls: calls.clone(),
            behavior,
            with_tool: false,
        });
        let (mut worker, _events) = DesktopRun::start(input).unwrap();
        let completion = drain(&mut worker, &mut owner, &mut writer, &target).await;
        assert!(completion.result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(completion.turn_suffix.is_empty());
        assert_eq!(completion.session.messages.len(), 2);
        writer
            .finish_with_messages_and_workflow(
                &target,
                &OperationId::new("main-fixture-run").unwrap(),
                Observation::Failed,
                ResultId::new("failed").unwrap(),
                completion.turn_suffix,
                None,
            )
            .unwrap();
        assert_eq!(writer.snapshot().unwrap().raw.len(), 2);
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "prompts/get")
                .count(),
            1
        );
        server_task.abort();
        let _ = server_task.await;
    }
}

#[tokio::test]
async fn cancel_during_metadata_or_get_joins_without_provider_or_resend() {
    for delayed in ["server/discover", "prompts/get"] {
        let (server, methods, server_task) = mcp_server::server_fixture(delayed).await;
        let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
        let (_root, mut writer, mut owner) = attach(&mut input, vec![server], None);
        let target = input.target.clone();
        let (mut worker, _events) = DesktopRun::start(input).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !methods.lock().unwrap().iter().any(|m| m == delayed) {
                owner.step(&mut writer, &target).unwrap();
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        worker.cancel();
        owner.cancel();
        let completion = drain(&mut worker, &mut owner, &mut writer, &target).await;
        let requested = match completion.result {
            Err(DesktopRunError::Agent(AgentError::Desktop(
                crate::desktop_events::EventFailure::Cancelled,
            ))) => Observation::Cancelled,
            // An ACKed unknown receipt is distinct from a dropped wait; both
            // must preserve the owner's OutcomeUnknown after an actual send.
            Err(DesktopRunError::PromptUnconfirmed) if delayed == "prompts/get" => {
                Observation::Failed
            }
            other => panic!("unexpected cancellation: {other:?}"),
        };
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(completion.turn_suffix.is_empty());
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "prompts/get")
                .count(),
            usize::from(delayed == "prompts/get")
        );
        let observation = owner.observation(requested);
        assert_eq!(
            observation,
            if delayed == "prompts/get" {
                Observation::OutcomeUnknown
            } else {
                Observation::Cancelled
            }
        );
        writer
            .finish_with_messages_and_workflow(
                &target,
                &OperationId::new("main-fixture-run").unwrap(),
                observation,
                ResultId::new("cancelled-final").unwrap(),
                completion.turn_suffix,
                None,
            )
            .unwrap();
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.state.runs[0].run.state, observation.state());
        assert_eq!(saved.raw.len(), 1 + usize::from(delayed == "prompts/get"));
        server_task.abort();
        let _ = server_task.await;
    }
}

#[test]
fn different_attempt_is_rejected_for_both_prepared_and_mcp_before_start() {
    for mcp in [false, true] {
        let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
        let _retained = mcp.then(|| attach(&mut input, vec![], None));
        input.target.attempt_id = AttemptId::new("different-attempt").unwrap();
        assert!(matches!(
            DesktopRun::start(input),
            Err(DesktopRunError::ExecutionMismatch)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn history_queue_cancellation_never_starts_provider_or_changes_raw() {
    let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
    let (_root, mut writer, mut owner) = attach(&mut input, vec![], None);
    let DesktopRequestContext::Mcp { prompt, .. } = &mut input.request_context else {
        unreachable!()
    };
    *prompt = None;
    let target = input.target.clone();
    let before = writer.snapshot().unwrap();
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    // Hold the owner before accepting the queued history read.
    tokio::time::timeout(Duration::from_secs(5), async {
        while owner.quiescent() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    worker.cancel();
    owner.cancel();
    let completion = drain(&mut worker, &mut owner, &mut writer, &target).await;
    assert!(matches!(
        completion.result,
        Err(DesktopRunError::Agent(AgentError::Desktop(
            crate::desktop_events::EventFailure::Cancelled
        )))
    ));
    assert_eq!(
        owner.observation(Observation::Cancelled),
        Observation::Cancelled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(writer.snapshot().unwrap().marker, before.marker);
    assert!(completion.turn_suffix.is_empty());
}

#[tokio::test]
async fn dropped_worker_during_prompt_get_releases_workspace_and_owner_without_resend() {
    let (server, methods, server_task) = mcp_server::server_fixture("prompts/get").await;
    let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
    let (_root, mut writer, mut owner) = attach(&mut input, vec![server], None);
    let target = input.target.clone();
    let prepared = Arc::downgrade(&input.prepared);
    let (worker, _events) = DesktopRun::start(input).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !methods.lock().unwrap().iter().any(|m| m == "prompts/get") {
            owner.step(&mut writer, &target).unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    drop(worker);
    tokio::time::timeout(Duration::from_secs(5), async {
        while prepared.upgrade().is_some() || !owner.quiescent() {
            owner.step(&mut writer, &target).unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        owner.observation(Observation::Cancelled),
        Observation::OutcomeUnknown
    );
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "prompts/get")
            .count(),
        1
    );
    // Drop deliberately loses the join receipt; this only proves resource release.
    server_task.abort();
    let _ = server_task.await;
}

#[test]
fn selected_prompt_without_registration_never_calls_provider() {
    let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
    let (_root, writer, owner) = attach(&mut input, vec![], None);
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert!(completion.result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(completion.turn_suffix.is_empty());
    assert_eq!(completion.session.messages.len(), 1);
    assert_eq!(writer.snapshot().unwrap().raw.len(), 1);
    assert!(owner.quiescent());
}
