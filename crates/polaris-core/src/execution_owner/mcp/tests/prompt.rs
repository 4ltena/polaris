//! Explicit prompt selection uses the same owner without model-call authority.
use super::*;
use polaris_desktop_protocol::{request::McpPromptChoice, run_state::RunState};
use polaris_provider::Role;
use std::future::pending;

fn choice() -> McpPromptChoice {
    McpPromptChoice {
        server: "fixture".into(),
        prompt: "prompt".into(),
        arguments: Default::default(),
    }
}
fn setup() -> (
    crate::desktop_store::PrototypeRoot,
    Writer,
    RunTarget,
    RunExecution,
    McpPort,
    tempfile::TempDir,
) {
    let target = RunTarget {
        run_id: RunId::new("prompt-run").unwrap(),
        attempt_id: AttemptId::new("attempt").unwrap(),
    };
    let (root, writer, target, mut owner, dir) =
        super::super::super::tests::read_owner_with_prompt(target, Some(choice()));
    let port = owner
        .configure_mcp(
            &target,
            SessionId::new("read-session").unwrap(),
            ExternalCeiling::Denied,
            ApprovalPolicy::Never,
        )
        .unwrap();
    (root, writer, target, owner, port, dir)
}
fn reply() -> serde_json::Value {
    json!({"messages":[{"role":"user","content":{"type":"text","text":"untrusted selected content"}}]})
}
fn selected(connection: &Connection) -> BoundOperation {
    let catalog = connection.catalog(1).unwrap();
    connection
        .host_select_prompt(1, &catalog.generation, "prompt", json!({}))
        .unwrap()
}
fn inject(point: Option<&'static str>) {
    crate::desktop_store::inject_test_failure(point);
}

#[tokio::test]
async fn history_lane_projection_failure_stops_owner_without_get_or_publication() {
    let (_root, mut writer, target, mut owner, port, _dir) = setup();
    let before = writer.snapshot().unwrap().marker;
    let mut history = Box::pin(port.history(pending()));
    assert!(history.as_mut().now_or_never().is_none());
    assert!(matches!(
        owner.step(&mut writer, &target),
        Err(StoreError::RunConflict)
    ));
    assert_eq!(history.await.unwrap_err(), McpCallError::Storage);
    assert!(owner.closed && port.stopped());
    assert!(owner.quiescent());
    assert_eq!(writer.snapshot().unwrap().marker, before);
    assert_eq!(owner.mcp.as_ref().unwrap().next_operation, 0);
}

#[tokio::test]
async fn history_lane_immediate_prompt_ack_waits_until_entry_releases_its_permit() {
    let (_root, mut writer, target, mut owner, port, _dir) = setup();
    let (mut connection, calls, fixture) = fixture::ready(reply(), false).await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = selected(&connection);
    let mut get = Box::pin(port.host_prompt(
        &mut connection,
        operation,
        "project".into(),
        &mut results,
        pending(),
        |_| true,
    ));
    assert!(get.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    owner.step(&mut writer, &target).unwrap();
    to_result_wait(&mut get, &owner).await;
    // Exercise the worker's legal interleaving between terminal ACK and the
    // entry's drop, without returning the permit before durable confirmation.
    let relay = owner.mcp.as_mut().unwrap();
    let mut entry = relay.entry.take().unwrap();
    let revision = writer.snapshot().unwrap().state.policy_revision;
    relay
        .advance(&mut entry, &mut writer, revision, &mut vec![])
        .unwrap();
    assert!(matches!(entry.phase, Phase::Done));
    assert!(get.await.unwrap().permits_continuation());
    let before = writer.snapshot().unwrap();
    let mut history = Box::pin(port.history(pending()));
    assert!(history.as_mut().now_or_never().is_none());
    assert!(relay.incoming.is_empty());
    drop(entry);
    assert!(history.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    let history = history.as_mut().now_or_never().unwrap().unwrap();
    assert_eq!(history.query(), "dummy");
    assert_eq!(history.messages().len(), 1);
    assert!(history.messages()[0].content.starts_with("dummy\n\n"));
    assert!(
        history.messages()[0]
            .content
            .contains("untrusted selected content")
    );
    assert_eq!(writer.snapshot().unwrap().marker, before.marker);
    assert_eq!(owner.mcp.as_ref().unwrap().next_operation, 1);
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(owner.quiescent());
    connection.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn host_prompt_relay_gets_once_after_intent_and_returns_only_saved_user_content() {
    let (root, mut writer, target, mut owner, port, _dir) = setup();
    let (mut connection, calls, fixture) = fixture::ready(reply(), false).await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = selected(&connection);
    let selection = operation.binding().selection_id.clone().unwrap();
    let mut get = Box::pin(port.host_prompt(
        &mut connection,
        operation,
        "project".into(),
        &mut results,
        pending(),
        |_| true,
    ));
    assert!(get.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    assert!(!owner.awaiting_approval());
    assert!(
        writer.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    owner.step(&mut writer, &target).unwrap();
    let begun = writer.snapshot().unwrap();
    assert_eq!(begun.raw.len(), 1);
    assert!(begun.state.runs[0].operations[0].prompt.is_some());
    assert!(begun.state.approval_records.is_empty());
    assert_eq!(calls.load(Ordering::Acquire), 0);
    to_result_wait(&mut get, &owner).await;
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(
        writer.snapshot().unwrap().state.runs[0].operations[0]
            .result_id
            .is_none()
    );
    owner.step(&mut writer, &target).unwrap();
    let result = get.await.unwrap();
    assert!(result.permits_continuation());
    assert!(owner.quiescent());
    assert_eq!(result.message.role, Role::User);
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 2);
    assert!(saved.raw[0].starts_turn && !saved.raw[1].starts_turn);
    assert_eq!(
        serde_json::to_value(&saved.raw[1].message).unwrap(),
        serde_json::to_value(&result.message).unwrap()
    );
    let content: serde_json::Value = serde_json::from_str(&result.message.content).unwrap();
    assert_eq!(content["reply"]["source"]["selection_id"], selection);
    assert!(content["reply"]["source"].get("model_call").is_none());
    writer
        .finish(
            &target,
            Observation::Succeeded,
            ResultId::new("finished").unwrap(),
        )
        .unwrap();
    connection.shutdown().await.unwrap();
    drop(writer);
    let writer = root
        .open(
            &ProjectId::new("project").unwrap(),
            &SessionId::new("read-session").unwrap(),
        )
        .unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.runs[0].run.state,
        RunState::Succeeded
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn host_prompt_relay_intent_ack_ambiguity_stops_even_when_publication_is_missing() {
    for point in ["sidecar_sync", "marker_rename", "directory_sync"] {
        let (root, mut writer, target, mut owner, port, _dir) = setup();
        let (mut connection, calls, fixture) = fixture::ready(reply(), false).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = selected(&connection);
        let mut get = Box::pin(port.host_prompt(
            &mut connection,
            operation,
            "project".into(),
            &mut results,
            pending(),
            |_| true,
        ));
        assert!(get.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        inject(Some(point));
        let step = owner.step(&mut writer, &target);
        inject(None);
        step.unwrap();
        assert_eq!(get.await.err(), Some(McpCallError::Storage));
        assert!(port.stopped());
        assert!(owner.closed);
        assert!(owner.quiescent());
        assert_eq!(
            owner.observation(Observation::Succeeded),
            Observation::Interrupted
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
        let saved = writer.snapshot().unwrap();
        let published = point == "directory_sync";
        assert_eq!(saved.raw.len(), if published { 2 } else { 1 });
        if published {
            assert_eq!(
                saved.state.runs[0].operations[0]
                    .prompt
                    .as_ref()
                    .unwrap()
                    .disposition(),
                Some(McpDisposition::BeforeSend)
            );
        }
        let retry = selected(&connection);
        assert!(
            port.host_prompt(
                &mut connection,
                retry,
                "project".into(),
                &mut results,
                pending(),
                |_| true
            )
            .await
            .is_err()
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
        connection.shutdown().await.unwrap();
        drop(writer);
        let writer = root
            .open(
                &ProjectId::new("project").unwrap(),
                &SessionId::new("read-session").unwrap(),
            )
            .unwrap();
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].run.state,
            RunState::Interrupted
        );
        fixture.abort();
        let _ = fixture.await;
    }
}

#[tokio::test]
async fn host_prompt_relay_terminal_ack_is_confirm_only_and_never_resends() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (root, mut writer, target, mut owner, port, _dir) = setup();
        let (mut connection, calls, fixture) = fixture::ready(reply(), false).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = selected(&connection);
        let mut get = Box::pin(port.host_prompt(
            &mut connection,
            operation,
            "project".into(),
            &mut results,
            pending(),
            |_| true,
        ));
        assert!(get.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        owner.step(&mut writer, &target).unwrap();
        to_result_wait(&mut get, &owner).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        inject(Some(point));
        let step = owner.step(&mut writer, &target);
        inject(None);
        let published = point == "directory_sync";
        assert_eq!(step.is_ok(), published);
        let result = get.await;
        if published {
            assert!(result.unwrap().permits_continuation());
        } else {
            assert!(matches!(result, Err(McpCallError::Storage)));
            assert!(port.stopped());
        }
        assert_eq!(owner.quiescent(), published);
        connection.shutdown().await.unwrap();
        drop(writer);
        let writer = root
            .open(
                &ProjectId::new("project").unwrap(),
                &SessionId::new("read-session").unwrap(),
            )
            .unwrap();
        let state = writer.snapshot().unwrap();
        assert_eq!(state.raw.len(), if published { 2 } else { 1 });
        assert_eq!(
            state.state.runs[0].run.state,
            if published {
                RunState::Interrupted
            } else {
                RunState::OutcomeUnknown
            }
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
        fixture.abort();
        let _ = fixture.await;
    }
}

#[tokio::test]
async fn host_prompt_relay_drop_or_cancel_preserves_send_progress_and_received_results() {
    for stage in [
        "before_grant",
        "before_send",
        "cancel_grant",
        "after_send",
        "after_reply",
    ] {
        let (root, mut writer, target, mut owner, port, _dir) = setup();
        let (mut connection, calls, fixture) = fixture::ready(reply(), stage == "after_send").await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = selected(&connection);
        let mut get = Box::pin(port.host_prompt(
            &mut connection,
            operation,
            "project".into(),
            &mut results,
            pending(),
            |_| true,
        ));
        assert!(get.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        if stage != "before_grant" {
            owner.step(&mut writer, &target).unwrap();
        }
        if stage == "after_send" {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    assert!(get.as_mut().now_or_never().is_none());
                    if calls.load(Ordering::Acquire) == 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
        } else if stage == "after_reply" {
            to_result_wait(&mut get, &owner).await;
        }
        if stage == "cancel_grant" {
            owner.cancel();
            assert_eq!(get.await.err(), Some(McpCallError::Cancelled));
        } else {
            drop(get);
        }
        owner.step(&mut writer, &target).unwrap();
        let expected = match stage {
            "before_grant" => None,
            "before_send" | "cancel_grant" => Some(McpDisposition::BeforeSend),
            "after_send" => Some(McpDisposition::OutcomeUnknown),
            _ => Some(McpDisposition::Completed),
        };
        let saved = writer.snapshot().unwrap();
        assert_eq!(
            saved.state.runs[0]
                .operations
                .first()
                .and_then(|op| op.prompt.as_ref())
                .and_then(|prompt| prompt.disposition()),
            expected
        );
        assert_eq!(saved.raw.len(), if expected.is_some() { 2 } else { 1 });
        assert!(owner.quiescent());
        let sent = matches!(stage, "after_send" | "after_reply");
        assert_eq!(calls.load(Ordering::Acquire), usize::from(sent));
        connection.shutdown().await.unwrap();
        drop(writer);
        let writer = root
            .open(
                &ProjectId::new("project").unwrap(),
                &SessionId::new("read-session").unwrap(),
            )
            .unwrap();
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].run.state,
            if stage == "after_send" {
                RunState::OutcomeUnknown
            } else {
                RunState::Interrupted
            }
        );
        assert_eq!(calls.load(Ordering::Acquire), usize::from(sent));
        fixture.abort();
        let _ = fixture.await;
    }
}

#[tokio::test]
async fn host_prompt_relay_before_send_input_required_and_unknown_halt_the_owner() {
    for expected in [
        McpDisposition::BeforeSend,
        McpDisposition::InputRequired,
        McpDisposition::OutcomeUnknown,
    ] {
        let (_root, mut writer, target, mut owner, port, _dir) = setup();
        let response = if expected == McpDisposition::InputRequired {
            json!({"resultType":"input_required","inputRequests":{},"requestState":"once"})
        } else {
            reply()
        };
        let (mut connection, calls, fixture) =
            fixture::ready(response, expected == McpDisposition::OutcomeUnknown).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = selected(&connection);
        let mut get = Box::pin(port.host_prompt(
            &mut connection,
            operation,
            "project".into(),
            &mut results,
            pending(),
            |_| expected != McpDisposition::BeforeSend,
        ));
        assert!(get.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        owner.step(&mut writer, &target).unwrap();
        to_result_wait(&mut get, &owner).await;
        owner.step(&mut writer, &target).unwrap();
        let result = get.await.unwrap();
        assert_eq!(result.disposition, expected);
        assert!(!result.permits_continuation());
        assert!(owner.closed && port.stopped());
        assert_eq!(
            owner.observation(Observation::Succeeded),
            if expected == McpDisposition::OutcomeUnknown {
                Observation::OutcomeUnknown
            } else {
                Observation::Interrupted
            }
        );
        assert_eq!(
            calls.load(Ordering::Acquire),
            usize::from(expected != McpDisposition::BeforeSend)
        );
        connection.shutdown().await.unwrap();
        fixture.abort();
        let _ = fixture.await;
    }
}

#[tokio::test]
async fn host_prompt_relay_rejects_unaccepted_arguments_and_foreign_project_before_intent() {
    for foreign_project in [false, true] {
        let (_root, mut writer, target, mut owner, port, _dir) = setup();
        let (mut connection, calls, fixture) = fixture::ready(reply(), false).await;
        let catalog = connection.catalog(1).unwrap();
        let operation = connection
            .host_select_prompt(
                1,
                &catalog.generation,
                "prompt",
                if foreign_project {
                    json!({})
                } else {
                    json!({"other":"argument"})
                },
            )
            .unwrap();
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let mut get = Box::pin(
            port.host_prompt(
                &mut connection,
                operation,
                if foreign_project {
                    "foreign"
                } else {
                    "project"
                }
                .into(),
                &mut results,
                pending(),
                |_| true,
            ),
        );
        assert!(get.as_mut().now_or_never().is_none());
        assert!(owner.step(&mut writer, &target).is_err());
        assert_eq!(get.await.err(), Some(McpCallError::Invalid));
        assert!(owner.closed && port.stopped());
        assert!(
            owner
                .port
                .submit(crate::desktop_execution::ExecutionCommand {
                    policy: polaris_sandbox::SandboxPolicy::new(
                        polaris_sandbox::SandboxMode::ReadOnly,
                        &[]
                    )
                    .unwrap(),
                    program: "/missing-must-not-execute".into(),
                    args: vec![],
                    stdin: None,
                })
                .is_err()
        );
        assert_eq!(
            owner.observation(Observation::Succeeded),
            Observation::Interrupted
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
        connection.shutdown().await.unwrap();
        fixture.abort();
        let _ = fixture.await;
    }
}
