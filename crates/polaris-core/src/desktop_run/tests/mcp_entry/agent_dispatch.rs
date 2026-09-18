//! Scripted model responses exercise the real worker, approval owner and raw store.
use super::*;
use polaris_desktop_protocol::{request::*, snapshot::ApprovalDecision};
use polaris_provider::{CompletionRequest, CompletionResponse, ToolCall};
use serde_json::{Value, json};

mod outcomes;

fn alias() -> String {
    polaris_mcp::catalog::tool_alias(&polaris_mcp::catalog::ItemId {
        server_id: "fixture".into(),
        kind: polaris_mcp::catalog::Kind::Tool,
        key: "fixture".into(),
    })
    .unwrap()
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn tool_body<'a>(request: &'a CompletionRequest, id: &str) -> &'a str {
    let messages = request
        .messages
        .iter()
        .filter(|m| m.tool_call_id.as_deref() == Some(id))
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 1, "each tool result occurs exactly once");
    &messages[0].content
}

struct Pipeline {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Provider for Pipeline {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.tools.iter().any(|s| s.name == "mcp_catalog"));
        assert_eq!(request.tools.iter().any(|s| s.name == alias()), index == 3);
        let tools = match index {
            0 => vec![call("servers", "mcp_catalog", json!({"op":"search"}))],
            1 => {
                let result: Value = serde_json::from_str(tool_body(&request, "servers")).unwrap();
                assert_eq!(result["unloaded_servers"], json!(["fixture"]));
                assert_eq!(result["partial"], true);
                vec![call(
                    "search",
                    "mcp_catalog",
                    json!({"op":"search","server":"fixture"}),
                )]
            }
            2 => {
                let found: Value = serde_json::from_str(tool_body(&request, "search")).unwrap();
                assert_eq!(found["partial"], false);
                assert_eq!(found["candidates_truncated"], false);
                let candidates = found["candidates"].as_array().unwrap();
                let tool = candidates.iter().find(|c| c["kind"] == "tool").unwrap();
                let resource = candidates.iter().find(|c| c["kind"] == "resource").unwrap();
                vec![
                    call(
                        "select",
                        "mcp_catalog",
                        json!({"op":"select","server":"fixture","kind":"tool","key":"fixture","generation":tool["generation"]}),
                    ),
                    call("premature", &alias(), json!({})),
                    call(
                        "read",
                        "mcp_catalog",
                        json!({"op":"read","server":"fixture","kind":"resource","key":resource["key"],"generation":resource["generation"]}),
                    ),
                ]
            }
            3 => {
                assert_eq!(tool_body(&request, "premature"), "mcp_invalid_selection");
                let resource: Value = serde_json::from_str(tool_body(&request, "read")).unwrap();
                assert_eq!(resource["disposition"], "completed");
                assert_eq!(resource["reply"]["parts"][0]["text"], "resource result");
                let found: Value = serde_json::from_str(tool_body(&request, "search")).unwrap();
                let template = found["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["kind"] == "resource_template")
                    .unwrap();
                vec![
                    call("remote", &alias(), json!({})),
                    call(
                        "template",
                        "mcp_catalog",
                        json!({"op":"read","server":"fixture","kind":"resource_template","key":template["key"],"generation":template["generation"],"arguments":{"id":"blue"}}),
                    ),
                ]
            }
            4 => {
                let remote: Value = serde_json::from_str(tool_body(&request, "remote")).unwrap();
                assert_eq!(remote["reply"]["status"], "complete");
                let handle = remote["reply"]["parts"][0]["handle"].as_str().unwrap();
                assert!(tool_body(&request, "template").contains("memory://note/blue"));
                vec![
                    call(
                        "chunk",
                        "mcp_catalog",
                        json!({"op":"chunk","handle":handle,"offset":0,"limit":1024}),
                    ),
                    call(
                        "forged-prompt",
                        "mcp_catalog",
                        json!({"op":"prompt","server":"fixture","key":"prompt"}),
                    ),
                ]
            }
            5 => {
                let chunk: Value = serde_json::from_str(tool_body(&request, "chunk")).unwrap();
                assert!(chunk["text"].as_str().unwrap().starts_with("known result"));
                assert!(chunk["next_offset"].as_u64().unwrap() > 0);
                assert_eq!(
                    tool_body(&request, "forged-prompt"),
                    "mcp_invalid_arguments"
                );
                return Ok(CompletionResponse {
                    text: "pipeline complete".into(),
                    ..Default::default()
                });
            }
            _ => panic!("unexpected extra provider request"),
        };
        Ok(CompletionResponse {
            tool_calls: tools,
            ..Default::default()
        })
    }
}

fn attach_without_prompt(
    input: &mut DesktopRunInput,
    server: Server,
) -> (PrototypeRoot, Writer, RunExecution) {
    let (root, writer, target) = history::writer_with_workflow(
        input.target.run_id.clone(),
        "fixture input",
        None,
        input.session.workflow.as_ref().map(SavedWorkflow::capture),
    );
    input.target = target.clone();
    let mut owner = RunExecution::new(&target, Arc::new(AtomicBool::new(false)));
    let port = owner
        .configure_mcp(
            &target,
            SessionId::new("desktop-session").unwrap(),
            ExternalCeiling::Allowed,
            ApprovalPolicy::Always,
        )
        .unwrap();
    input.request_context = DesktopRequestContext::Mcp {
        config: Box::new(
            RuntimeConfig::new(
                Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![server],
                },
                "project".into(),
                Arc::new(Secrets::new(1, BTreeMap::new()).unwrap()),
                BTreeMap::new(),
            )
            .unwrap(),
        ),
        port,
        prompt: None,
        preselection: Default::default(),
    };
    input.max_turns = 8;
    (root, writer, owner)
}

fn approve(writer: &mut Writer, owner: &mut RunExecution) -> usize {
    let saved = writer.snapshot().unwrap();
    let pending = saved.state.unresolved_approvals;
    let count = pending.len();
    for approval in pending {
        writer
            .resolve_approval(
                &Request {
                    protocol_version: Default::default(),
                    client_id: ClientId::new("mcp-agent").unwrap(),
                    request_id: RequestId::new(format!("allow-{}", approval.approval_id.as_str()))
                        .unwrap(),
                    body: RequestBody::ApprovalResolve(
                        saved.marker.session_id.clone(),
                        ApprovalResolve {
                            approval_id: approval.approval_id.clone(),
                            run_id: approval.run_id,
                            attempt_id: approval.attempt_id,
                            policy_revision: approval.policy_revision,
                            decision: ApprovalDecision::Allow,
                        },
                    ),
                },
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            )
            .unwrap();
        owner.approval_resolved(&approval.approval_id, ApprovalDecision::Allow);
    }
    count
}

async fn drain_with_approval(
    worker: &mut DesktopRun,
    owner: &mut RunExecution,
    writer: &mut Writer,
    target: &RunTarget,
) -> (DesktopRunCompletion, usize) {
    drain_with_workflow(worker, owner, writer, target, None).await
}

async fn drain_with_workflow(
    worker: &mut DesktopRun,
    owner: &mut RunExecution,
    writer: &mut Writer,
    target: &RunTarget,
    mut workflow: Option<&mut crate::desktop_workflow::DesktopWorkflowOwner>,
) -> (DesktopRunCompletion, usize) {
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut approvals = 0;
        loop {
            owner.step(writer, target).unwrap();
            approvals += approve(writer, owner);
            if let Some(workflow) = workflow.as_deref_mut() {
                workflow.poll(Some(writer), false, None);
            }
            if let Some(completion) = worker.try_join() {
                assert!(owner.quiescent());
                return (completion.unwrap(), approvals);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("MCP model pipeline did not finish")
}

#[tokio::test]
async fn agent_search_select_call_read_and_chunk_keep_request_schema_and_canonical_raw() {
    let (server, methods, server_task) = mcp_server::server_fixture("").await;
    let (_temp, mut input, _unused, calls) = fixture(Behavior::Complete);
    let (root, mut writer, mut owner) = attach_without_prompt(&mut input, server);
    let target = input.target.clone();
    input.provider = Arc::new(Pipeline {
        calls: calls.clone(),
    });
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let (completion, approvals) =
        drain_with_approval(&mut worker, &mut owner, &mut writer, &target).await;
    assert!(completion.result.is_ok(), "{:?}", completion.result.err());
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(
        approvals, 1,
        "resources and local chunks require no tool approval"
    );
    assert_eq!(completion.turn_suffix.len(), 15);
    writer
        .finish_with_messages_and_workflow(
            &target,
            &OperationId::new("main-fixture-run").unwrap(),
            Observation::Succeeded,
            ResultId::new("pipeline-final").unwrap(),
            completion.turn_suffix.clone(),
            None,
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 16);
    let expected: Vec<_> = saved.raw.iter().map(|event| &event.message).collect();
    assert_eq!(
        serde_json::to_value(&completion.session.messages).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        saved.state.runs[0]
            .operations
            .iter()
            .filter(|op| op.mcp.is_some())
            .count(),
        3
    );
    let observed = methods.lock().unwrap().clone();
    assert_eq!(
        observed
            .iter()
            .filter(|m| m.as_str() == "tools/call")
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|m| m.as_str() == "resources/read")
            .count(),
        2
    );
    assert!(!observed.iter().any(|m| m == "prompts/get"));
    drop(writer);
    let reopened = root
        .open(
            &ProjectId::new("project").unwrap(),
            &SessionId::new("desktop-session").unwrap(),
        )
        .unwrap();
    assert_eq!(reopened.snapshot().unwrap().marker, saved.marker);
    assert_eq!(*methods.lock().unwrap(), observed);
    server_task.abort();
    let _ = server_task.await;
}

struct Selector(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Provider for Selector {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(request.tools.is_empty());
        assert!(request.messages[0].content.contains("fixture input"));
        Ok(CompletionResponse {
            text: r#"{"tools":[0]}"#.into(),
            ..Default::default()
        })
    }
}

struct Preselected(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Provider for Preselected {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(request.tools.iter().any(|s| s.name == alias()));
        assert!(!request.system.contains("candidates"));
        Ok(CompletionResponse {
            text: "preselected".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn host_preselection_offers_the_tool_without_model_selection_and_audits_status() {
    let (server, methods, server_task) = mcp_server::server_fixture("").await;
    let (temp, mut input, _unused, calls) = fixture(Behavior::Complete);
    let (_root, mut writer, mut owner) = attach_without_prompt(&mut input, server);
    let index = {
        let DesktopRequestContext::Mcp { config, .. } = &input.request_context else {
            panic!("MCP context");
        };
        let probe = RunTarget {
            run_id: RunId::new("probe").unwrap(),
            attempt_id: AttemptId::new("probe").unwrap(),
        };
        let mut probe_owner = RunExecution::new(&probe, Arc::new(AtomicBool::new(false)));
        let port = probe_owner
            .configure_mcp(
                &probe,
                SessionId::new("probe").unwrap(),
                ExternalCeiling::Denied,
                ApprovalPolicy::Never,
            )
            .unwrap();
        let mut runtime = crate::mcp::runtime::RunRuntime::new((**config).clone(), port).unwrap();
        let entry = runtime
            .index_entry("fixture", std::future::pending())
            .await
            .unwrap();
        runtime.shutdown().await.unwrap();
        BTreeMap::from([("fixture".to_owned(), entry)])
    };
    let selections = Arc::new(AtomicUsize::new(0));
    let DesktopRequestContext::Mcp { preselection, .. } = &mut input.request_context else {
        panic!("MCP context");
    };
    *preselection = crate::desktop_run::McpPreselection {
        index: Ok(Some(index)),
        selector: Some(Arc::new(Selector(selections.clone()))),
        decider: None,
    };
    let target = input.target.clone();
    input.provider = Arc::new(Preselected(calls.clone()));
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let (completion, approvals) =
        drain_with_approval(&mut worker, &mut owner, &mut writer, &target).await;
    assert!(completion.result.is_ok(), "{:?}", completion.result.err());
    assert_eq!(approvals, 0);
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let audit = std::fs::read_to_string(temp.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains("mcp-preselection") && audit.contains("selected"));
    assert!(!methods.lock().unwrap().iter().any(|m| m == "tools/call"));
    server_task.abort();
    let _ = server_task.await;
}
