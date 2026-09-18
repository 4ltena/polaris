//! Failure outcomes must retain canonical evidence and control continuation.
use super::*;
use crate::{
    desktop_workflow::DesktopWorkflow,
    workflow::{Phase, SessionWorkflow, VerificationEvidence, WorkflowConfig, WorkflowMode},
};

struct OutcomeProvider {
    calls: Arc<AtomicUsize>,
    expected_status: &'static str,
    workflow: bool,
}

#[async_trait::async_trait]
impl Provider for OutcomeProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let tools = match index {
            0 => vec![call(
                "search",
                "mcp_catalog",
                json!({"op":"search","server":"fixture","kind":"tool"}),
            )],
            1 => {
                let search: Value = serde_json::from_str(tool_body(&request, "search")).unwrap();
                vec![call(
                    "select",
                    "mcp_catalog",
                    json!({"op":"select","server":"fixture","kind":"tool","key":"fixture","generation":search["candidates"][0]["generation"]}),
                )]
            }
            2 => {
                assert!(request.tools.iter().any(|tool| tool.name == alias()));
                let mut tools = Vec::new();
                if self.workflow {
                    // Defer this before the remote call, so response ordering
                    // cannot hide a failed sibling or a newly stale proof.
                    tools.push(call(
                        "transition",
                        "state",
                        json!({"op":"transition","phase":"implement","expected_revision":0}),
                    ));
                }
                tools.push(call("remote", &alias(), json!({})));
                tools.push(call("after-remote", "mcp_catalog", json!({"op":"search"})));
                tools
            }
            3 => {
                assert!(matches!(
                    self.expected_status,
                    "complete" | "tool_error" | "before_send"
                ));
                let remote: Value = serde_json::from_str(tool_body(&request, "remote")).unwrap();
                if self.expected_status == "before_send" {
                    assert_eq!(remote["disposition"], "before_send");
                } else {
                    assert_eq!(remote["reply"]["status"], self.expected_status);
                    assert_eq!(remote["disposition"], "completed");
                }
                assert!(!tool_body(&request, "after-remote").is_empty());
                if self.workflow {
                    let transition: Value =
                        serde_json::from_str(tool_body(&request, "transition")).unwrap();
                    assert_eq!(
                        transition["status"],
                        if self.expected_status != "complete" {
                            "not_applied"
                        } else {
                            "applied"
                        }
                    );
                    vec![call("state-after", "state", json!({"op":"get"}))]
                } else {
                    return Ok(CompletionResponse {
                        text: "failure handled".into(),
                        ..Default::default()
                    });
                }
            }
            4 if self.workflow => {
                let state: Value =
                    serde_json::from_str(tool_body(&request, "state-after")).unwrap();
                assert_eq!(state["verification"], "stale");
                assert_eq!(
                    state["phase"],
                    if self.expected_status == "complete" {
                        "implement"
                    } else {
                        "review"
                    }
                );
                return Ok(CompletionResponse {
                    text: "workflow checked".into(),
                    ..Default::default()
                });
            }
            _ => panic!("provider continued after an unconfirmed remote result"),
        };
        Ok(CompletionResponse {
            tool_calls: tools,
            ..Default::default()
        })
    }
}

fn remote_reply(status: &str) -> Value {
    match status {
        "tool_error" => {
            json!({"content":[{"type":"text","text":"synthetic tool failure"}],"isError":true})
        }
        "input_required" => {
            json!({"resultType":"input_required","inputRequests":{},"requestState":"one-shot"})
        }
        _ => json!({"content":[{"type":"text","text":"synthetic completion"}]}),
    }
}

#[tokio::test]
async fn canonical_failure_stops_only_unconfirmed_outcomes_and_reopen_never_resends() {
    for status in ["tool_error", "input_required", "outcome_unknown"] {
        let (server, methods, server_task) = mcp_server::server_fixture_with_tool_reply(
            if status == "outcome_unknown" {
                "tools/call"
            } else {
                ""
            },
            remote_reply(status),
        )
        .await;
        let (_temp, mut input, _unused, calls) = fixture(Behavior::Complete);
        let (root, mut writer, mut owner) = attach_without_prompt(&mut input, server);
        let target = input.target.clone();
        input.provider = Arc::new(OutcomeProvider {
            calls: calls.clone(),
            expected_status: status,
            workflow: false,
        });
        let (mut worker, _events) = DesktopRun::start(input).unwrap();
        let (completion, approvals) =
            drain_with_approval(&mut worker, &mut owner, &mut writer, &target).await;
        assert_eq!(approvals, 1);
        let confirmed = status == "tool_error";
        assert_eq!(calls.load(Ordering::SeqCst), if confirmed { 4 } else { 3 });
        if confirmed {
            assert!(completion.result.is_ok(), "{:?}", completion.result.err());
        } else {
            assert!(
                matches!(
                    completion.result,
                    Err(DesktopRunError::Agent(AgentError::McpUnconfirmed))
                ),
                "{:?}",
                completion.result.err()
            );
            assert!(
                !completion
                    .turn_suffix
                    .iter()
                    .any(|m| m.tool_call_id.as_deref() == Some("after-remote"))
            );
        }
        let canonical = completion
            .turn_suffix
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("remote"))
            .collect::<Vec<_>>();
        assert_eq!(canonical.len(), 1);
        let envelope: Value = serde_json::from_str(&canonical[0].content).unwrap();
        assert_eq!(
            envelope["disposition"],
            if confirmed {
                "completed"
            } else if status == "input_required" {
                "input_required"
            } else {
                "outcome_unknown"
            }
        );
        // Canonical evidence is already durable before final run publication.
        let before_finish = writer.snapshot().unwrap();
        let saved_remote: Vec<_> = before_finish
            .raw
            .iter()
            .filter(|e| e.message.tool_call_id.as_deref() == Some("remote"))
            .collect();
        assert_eq!(saved_remote.len(), 1);
        assert_eq!(
            serde_json::to_value(&saved_remote[0].message).unwrap(),
            serde_json::to_value(canonical[0]).unwrap()
        );
        let observation = owner.observation(if confirmed {
            Observation::Succeeded
        } else {
            Observation::Failed
        });
        if status == "outcome_unknown" {
            assert_eq!(observation, Observation::OutcomeUnknown);
        }
        writer
            .finish_with_messages_and_workflow(
                &target,
                &OperationId::new("main-fixture-run").unwrap(),
                observation,
                ResultId::new("outcome-final").unwrap(),
                completion.turn_suffix,
                None,
            )
            .unwrap();
        let saved = writer.snapshot().unwrap();
        assert_eq!(
            saved.state.runs[0]
                .operations
                .iter()
                .filter(|op| op.mcp.is_some())
                .count(),
            1
        );
        let observed = methods.lock().unwrap().clone();
        assert_eq!(
            observed
                .iter()
                .filter(|method| method.as_str() == "tools/call")
                .count(),
            1
        );
        drop(writer);
        let reopened = root
            .open(
                &ProjectId::new("project").unwrap(),
                &SessionId::new("desktop-session").unwrap(),
            )
            .unwrap();
        let recovered = reopened.snapshot().unwrap();
        assert_eq!(recovered.marker, saved.marker);
        let expected_observation = match status {
            "input_required" => Observation::Interrupted,
            "outcome_unknown" => Observation::OutcomeUnknown,
            _ => Observation::Succeeded,
        };
        assert_eq!(saved.state.runs[0].run.state, expected_observation.state());
        assert_eq!(
            recovered.state.runs[0].run.state,
            expected_observation.state()
        );
        assert_eq!(
            serde_json::to_value(recovered.raw).unwrap(),
            serde_json::to_value(saved.raw).unwrap()
        );
        assert_eq!(*methods.lock().unwrap(), observed);
        server_task.abort();
        let _ = server_task.await;
    }
}

#[tokio::test]
async fn catalog_expiring_during_approval_publishes_before_send_without_success_or_replay() {
    let (mut server, methods, server_task) =
        mcp_server::server_fixture_with_tool_reply("", remote_reply("complete")).await;
    server.timeouts.catalog_ttl_seconds = 30;
    let (_temp, mut input, _unused, calls) = fixture(Behavior::Complete);
    let mut workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        mode: WorkflowMode::Adaptive,
        initial_phase: Phase::Review,
        ..Default::default()
    });
    workflow.gates.approved_spec_hash = Some("approved-fixture".into());
    workflow.gates.verification = Some(VerificationEvidence {
        artifact_hash: "fixture-artifact".into(),
        passed: true,
        stale: false,
    });
    input.session.workflow = Some(workflow);
    let (_root, mut writer, mut owner) = attach_without_prompt(&mut input, server);
    let target = input.target.clone();
    let (port, mut workflow_owner) = DesktopWorkflow::pair(target.clone());
    input.session.desktop_workflow = Some(port);
    input.provider = Arc::new(OutcomeProvider {
        calls: calls.clone(),
        expected_status: "before_send",
        workflow: true,
    });
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let mut approvals = 0;
    let completion = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            owner.step(&mut writer, &target).unwrap();
            workflow_owner.poll(Some(&mut writer), false, None);
            if !writer
                .snapshot()
                .unwrap()
                .state
                .unresolved_approvals
                .is_empty()
            {
                assert_eq!(approvals, 0);
                // The model selected and dispatched while the catalog was
                // valid. Expire it only while the actual owner awaits approval.
                tokio::time::sleep(Duration::from_secs(31)).await;
                approvals += approve(&mut writer, &mut owner);
            }
            if let Some(completion) = worker.try_join() {
                assert!(owner.quiescent());
                break completion.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("expiry fixture did not finish");
    assert!(completion.result.is_ok(), "{:?}", completion.result.err());
    assert_eq!(approvals, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert!(
        !methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "tools/call")
    );
    let saved = writer.snapshot().unwrap();
    let canonical: Vec<_> = saved
        .raw
        .iter()
        .filter(|e| e.message.tool_call_id.as_deref() == Some("remote"))
        .collect();
    assert_eq!(canonical.len(), 1);
    let value: Value = serde_json::from_str(&canonical[0].message.content).unwrap();
    assert_eq!(value["disposition"], "before_send");
    let candidate = SavedWorkflow::capture(completion.session.workflow.as_ref().unwrap());
    assert_eq!(candidate.state.phase, Phase::Review);
    assert!(candidate.gates.verification.as_ref().unwrap().stale);
    writer
        .finish_with_messages_and_workflow(
            &target,
            &OperationId::new("main-fixture-run").unwrap(),
            Observation::Succeeded,
            ResultId::new("before-send-final").unwrap(),
            completion.turn_suffix,
            Some(candidate),
        )
        .unwrap();
    server_task.abort();
    let _ = server_task.await;
}

struct TruncatedSearch;
#[async_trait::async_trait]
impl Provider for TruncatedSearch {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        if !request
            .messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("search"))
        {
            return Ok(CompletionResponse {
                tool_calls: vec![call(
                    "search",
                    "mcp_catalog",
                    json!({"op":"search","server":"fixture","kind":"tool"}),
                )],
                ..Default::default()
            });
        }
        let value: Value = serde_json::from_str(tool_body(&request, "search")).unwrap();
        assert_eq!(value["unloaded_servers"], json!([]));
        assert_eq!(value["rejected_items"], 0);
        assert_eq!(value["partial"], true);
        assert_eq!(value["candidates_truncated"], true);
        let candidates = value["candidates"].as_array().unwrap();
        assert!(!candidates.is_empty());
        assert!(candidates.len() <= 12);
        Ok(CompletionResponse {
            text: "partial catalog observed".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn search_reports_candidate_truncation_even_when_all_catalogs_loaded() {
    let (server, _methods, server_task) = mcp_server::server_fixture_with_replies(
        "",
        json!({"content":[]}),
        json!(
            (0..13)
                .map(|i| json!({"name":format!("tool-{i}"),"inputSchema":{"type":"object"}}))
                .collect::<Vec<_>>()
        ),
    )
    .await;
    let (_temp, mut input, _unused, _calls) = fixture(Behavior::Complete);
    let (_root, mut writer, mut owner) = attach_without_prompt(&mut input, server);
    let target = input.target.clone();
    input.provider = Arc::new(TruncatedSearch);
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let (completion, approvals) =
        drain_with_approval(&mut worker, &mut owner, &mut writer, &target).await;
    assert!(completion.result.is_ok(), "{:?}", completion.result.err());
    assert_eq!(approvals, 0);
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn remote_tool_stales_verification_and_failed_sibling_prevents_workflow_transition() {
    for status in ["complete", "tool_error"] {
        let (server, _methods, server_task) =
            mcp_server::server_fixture_with_tool_reply("", remote_reply(status)).await;
        let (_temp, mut input, _unused, calls) = fixture(Behavior::Complete);
        let mut workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            initial_phase: Phase::Review,
            ..Default::default()
        });
        workflow.gates.approved_spec_hash = Some("approved-fixture".into());
        workflow.gates.verification = Some(VerificationEvidence {
            artifact_hash: "fixture-artifact".into(),
            passed: true,
            stale: false,
        });
        input.session.workflow = Some(workflow);
        let (_root, mut writer, mut owner) = attach_without_prompt(&mut input, server);
        let target = input.target.clone();
        let (port, mut workflow_owner) = DesktopWorkflow::pair(target.clone());
        input.session.desktop_workflow = Some(port);
        input.provider = Arc::new(OutcomeProvider {
            calls: calls.clone(),
            expected_status: status,
            workflow: true,
        });
        let (mut worker, _events) = DesktopRun::start(input).unwrap();
        let (completion, approvals) = drain_with_workflow(
            &mut worker,
            &mut owner,
            &mut writer,
            &target,
            Some(&mut workflow_owner),
        )
        .await;
        assert!(completion.result.is_ok(), "{:?}", completion.result.err());
        assert_eq!(approvals, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        let candidate = SavedWorkflow::capture(completion.session.workflow.as_ref().unwrap());
        assert!(candidate.gates.verification.as_ref().unwrap().stale);
        assert_eq!(
            candidate.state.phase,
            if status == "complete" {
                Phase::Implement
            } else {
                Phase::Review
            }
        );
        writer
            .finish_with_messages_and_workflow(
                &target,
                &OperationId::new("main-fixture-run").unwrap(),
                Observation::Succeeded,
                ResultId::new("workflow-final").unwrap(),
                completion.turn_suffix,
                Some(candidate.clone()),
            )
            .unwrap();
        assert_eq!(writer.snapshot().unwrap().state.workflow, Some(candidate));
        server_task.abort();
        let _ = server_task.await;
    }
}
