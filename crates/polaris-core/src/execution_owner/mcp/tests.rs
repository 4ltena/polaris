//! Real owner/Writer and loopback transport; no model or production server.
use super::*;
use crate::execution_owner::RunExecution;
use polaris_desktop_protocol::{
    ids::*,
    request::{ApprovalResolve, Request as WireRequest, RequestBody},
};
use polaris_provider::ToolCall;
use serde_json::json;
use std::pin::Pin;
mod fixture;
mod history;
mod prompt;

fn configured(owner: &mut RunExecution, target: &RunTarget) -> McpPort {
    owner
        .configure_mcp(
            target,
            SessionId::new("read-session").unwrap(),
            ExternalCeiling::Allowed,
            ApprovalPolicy::Always,
        )
        .unwrap()
}

fn call(connection: &Connection) -> McpCall {
    let catalog = connection.catalog(1).unwrap();
    let item = &catalog.items[0];
    let bound = connection
        .prepare_operation(1, &catalog.generation, &item.id, json!({}))
        .unwrap();
    McpCall::new(
        bound,
        "model-call".into(),
        "synthetic project".into(),
        vec![Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "model-call".into(),
                name: polaris_mcp::catalog::tool_alias(&item.id).unwrap(),
                arguments: json!({}),
            }],
        )],
    )
    .unwrap()
}

fn decide(writer: &mut Writer, owner: &mut RunExecution, decision: ApprovalDecision) -> ApprovalId {
    let pending = owner
        .mcp
        .as_ref()
        .unwrap()
        .entry
        .as_ref()
        .unwrap()
        .approval
        .clone()
        .unwrap();
    // The card shows the exact hashed arguments.
    assert!(pending.display.description.ends_with("\n引数: {}"));
    writer
        .resolve_approval(
            &WireRequest {
                protocol_version: Default::default(),
                client_id: ClientId::new("mcp-test").unwrap(),
                request_id: RequestId::new("decision").unwrap(),
                body: RequestBody::ApprovalResolve(
                    SessionId::new("read-session").unwrap(),
                    ApprovalResolve {
                        approval_id: pending.approval_id.clone(),
                        run_id: pending.run_id,
                        attempt_id: pending.attempt_id,
                        policy_revision: pending.policy_revision,
                        decision,
                    },
                ),
            },
            now_ms().unwrap(),
        )
        .unwrap();
    owner.approval_resolved(&pending.approval_id, decision);
    pending.approval_id
}

async fn to_result_wait<T, F: Future<Output = Result<T, McpCallError>>>(
    future: &mut Pin<Box<F>>,
    owner: &RunExecution,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(
                future.as_mut().now_or_never().is_none(),
                "caller passed the durable ACK boundary"
            );
            if owner
                .mcp
                .as_ref()
                .unwrap()
                .entry
                .as_ref()
                .unwrap()
                .request
                .shared
                .terminal
                .lock()
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn mcp_relay_sends_only_after_intent_and_releases_only_after_result_publication() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    let (mut connection, calls, server) =
        fixture::ready(json!({"content":[{"type":"text","text":"done"}]}), false).await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = call(&connection);
    let mut future = Box::pin(port.call(
        &mut connection,
        operation,
        &mut results,
        std::future::pending(),
        |_| true,
    ));
    assert!(future.as_mut().now_or_never().is_none());
    assert!(!owner.quiescent());
    owner.step(&mut writer, &target).unwrap();
    assert!(owner.awaiting_approval());
    assert!(future.as_mut().now_or_never().is_none());
    assert!(
        !owner
            .mcp
            .as_ref()
            .unwrap()
            .entry
            .as_ref()
            .unwrap()
            .request
            .progress
            .may_have_sent()
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert!(
        writer.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );
    let approval = decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(writer.snapshot().unwrap().state.runs[0].operations.len(), 1);
    assert_eq!(calls.load(Ordering::Acquire), 0);
    owner.approval_resolved(&approval, ApprovalDecision::Allow);
    to_result_wait(&mut future, &owner).await;
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(!owner.quiescent());
    assert!(
        writer.snapshot().unwrap().state.runs[0].operations[0]
            .result_id
            .is_none()
    );
    owner.step(&mut writer, &target).unwrap();
    let result = future.await.unwrap();
    assert!(result.permits_continuation());
    assert_eq!(result.disposition, McpDisposition::Completed);
    assert!(owner.quiescent());
    assert_eq!(writer.snapshot().unwrap().raw.len(), 3);
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    connection.shutdown().await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mcp_relay_pending_rejections_never_create_intent_or_send() {
    for reason in ["deny", "drop", "cancel", "disconnect", "policy", "expiry"] {
        let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
        let port = configured(&mut owner, &target);
        let (mut connection, calls, server) = fixture::ready(json!({"content":[]}), false).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = call(&connection);
        let mut future = Box::pin(port.call(
            &mut connection,
            operation,
            &mut results,
            std::future::pending(),
            |_| true,
        ));
        assert!(future.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        match reason {
            "deny" => {
                decide(&mut writer, &mut owner, ApprovalDecision::Deny);
            }
            "cancel" => owner.cancel(),
            "disconnect" => owner.disconnected.store(true, Ordering::Release),
            "policy" => writer.set_policy_revision(DecimalU64::new(1)).unwrap(),
            "expiry" => {
                owner
                    .mcp
                    .as_mut()
                    .unwrap()
                    .entry
                    .as_mut()
                    .unwrap()
                    .deadline
                    .monotonic = Instant::now()
            }
            _ => (),
        }
        if reason == "drop" {
            drop(future);
            owner.step(&mut writer, &target).unwrap();
        } else {
            owner.step(&mut writer, &target).unwrap();
            assert!(future.await.is_err());
        }
        assert!(owner.quiescent(), "{reason}");
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(writer.snapshot().unwrap().raw.len(), 1);
        if reason == "policy" {
            assert!(owner.take_approval_events().iter().any(|event| matches!(
                event,
                EventBody::ApprovalExpired(ApprovalExpired {
                    reason: ExpiryReason::PolicyRevoked,
                    ..
                })
            )));
        }
        connection.shutdown().await.unwrap();
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mcp_relay_intent_faults_confirm_presence_without_issuing_a_grant() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
        let port = configured(&mut owner, &target);
        let (mut connection, calls, server) = fixture::ready(json!({"content":[]}), false).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = call(&connection);
        let mut future = Box::pin(port.call(
            &mut connection,
            operation,
            &mut results,
            std::future::pending(),
            |_| true,
        ));
        assert!(future.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        decide(&mut writer, &mut owner, ApprovalDecision::Allow);
        crate::desktop_store::inject_test_failure(Some(point));
        let result = owner.step(&mut writer, &target);
        crate::desktop_store::inject_test_failure(None);
        result.unwrap();
        assert!(matches!(future.await, Err(McpCallError::Storage)));
        assert!(owner.quiescent());
        assert_eq!(calls.load(Ordering::Acquire), 0);
        let state = writer.snapshot().unwrap();
        if point == "directory_sync" {
            assert_eq!(state.raw.len(), 3);
            assert_eq!(
                state.state.runs[0].operations[0]
                    .mcp
                    .as_ref()
                    .unwrap()
                    .disposition(),
                Some(McpDisposition::BeforeSend)
            );
        } else {
            assert!(state.state.runs[0].operations.is_empty());
            assert_eq!(state.raw.len(), 1);
        }
        connection.shutdown().await.unwrap();
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mcp_relay_dropped_or_cancelled_grants_distinguish_unsent_and_unknown_after_one_call() {
    for (sent, cancel_grant) in [(false, false), (false, true), (true, false)] {
        let (root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
        let port = configured(&mut owner, &target);
        let (mut connection, calls, server) = fixture::ready(json!({"content":[]}), true).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = call(&connection);
        let mut future = Box::pin(port.call(
            &mut connection,
            operation,
            &mut results,
            std::future::pending(),
            |_| true,
        ));
        assert!(future.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        decide(&mut writer, &mut owner, ApprovalDecision::Allow);
        owner.step(&mut writer, &target).unwrap();
        if sent {
            tokio::time::timeout(Duration::from_secs(2), async {
                while calls.load(Ordering::Acquire) == 0 {
                    assert!(future.as_mut().now_or_never().is_none());
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
        }
        if cancel_grant {
            owner.cancel();
            assert_eq!(future.await.err(), Some(McpCallError::Cancelled));
        } else {
            drop(future);
        }
        owner.step(&mut writer, &target).unwrap();
        let expected = if sent {
            McpDisposition::OutcomeUnknown
        } else {
            McpDisposition::BeforeSend
        };
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].operations[0]
                .mcp
                .as_ref()
                .unwrap()
                .disposition(),
            Some(expected)
        );
        assert_eq!(calls.load(Ordering::Acquire), usize::from(sent));
        assert!(owner.quiescent());
        if sent {
            assert_eq!(
                owner.observation(Observation::Cancelled),
                Observation::OutcomeUnknown
            );
        }
        connection.shutdown().await.unwrap();
        server.abort();
        let _ = server.await;
        drop(owner);
        drop(writer);
        let reopened = root
            .open(
                &polaris_desktop_protocol::ids::ProjectId::new("project").unwrap(),
                &SessionId::new("read-session").unwrap(),
            )
            .unwrap();
        assert_eq!(reopened.snapshot().unwrap().raw.len(), 3);
        assert_eq!(calls.load(Ordering::Acquire), usize::from(sent));
    }
}

#[tokio::test]
async fn mcp_relay_received_results_survive_client_drop_and_stop_when_incomplete() {
    for input_required in [false, true] {
        let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
        let port = configured(&mut owner, &target);
        let reply = if input_required {
            json!({"resultType":"input_required","inputRequests":{},"requestState":"once"})
        } else {
            json!({"content":[{"type":"text","text":"received"}]})
        };
        let (mut connection, calls, server) = fixture::ready(reply, false).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = call(&connection);
        let mut future = Box::pin(port.call(
            &mut connection,
            operation,
            &mut results,
            std::future::pending(),
            |_| true,
        ));
        assert!(future.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        decide(&mut writer, &mut owner, ApprovalDecision::Allow);
        owner.step(&mut writer, &target).unwrap();
        to_result_wait(&mut future, &owner).await;
        drop(future);
        owner.step(&mut writer, &target).unwrap();
        let snapshot = writer.snapshot().unwrap();
        let messages: Vec<_> = snapshot
            .raw
            .iter()
            .map(|event| &event.message)
            .filter(|message| message.tool_call_id.as_deref() == Some("model-call"))
            .collect();
        assert_eq!(messages.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        if input_required {
            assert_eq!(body["reply"]["parts"][0]["request"]["requestState"], "once");
        } else {
            assert_eq!(body["reply"]["parts"][0]["text"], "received");
        }
        let expected = if input_required {
            McpDisposition::InputRequired
        } else {
            McpDisposition::Completed
        };
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].operations[0]
                .mcp
                .as_ref()
                .unwrap()
                .disposition(),
            Some(expected)
        );
        assert_eq!(
            owner.observation(Observation::Succeeded),
            if input_required {
                Observation::Interrupted
            } else {
                Observation::Succeeded
            }
        );
        assert!(owner.quiescent());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        connection.shutdown().await.unwrap();
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mcp_relay_terminal_faults_retain_publication_and_never_repeat_transport() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
        let port = configured(&mut owner, &target);
        let (mut connection, calls, server) = fixture::ready(json!({"content":[]}), false).await;
        let mut results = ResultStore::new(port.binding.clone()).unwrap();
        let operation = call(&connection);
        let mut future = Box::pin(port.call(
            &mut connection,
            operation,
            &mut results,
            std::future::pending(),
            |_| true,
        ));
        assert!(future.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        decide(&mut writer, &mut owner, ApprovalDecision::Allow);
        owner.step(&mut writer, &target).unwrap();
        to_result_wait(&mut future, &owner).await;
        crate::desktop_store::inject_test_failure(Some(point));
        let result = owner.step(&mut writer, &target);
        crate::desktop_store::inject_test_failure(None);
        if point == "directory_sync" {
            result.unwrap();
            assert!(future.await.unwrap().permits_continuation());
            assert!(owner.quiescent());
        } else {
            assert!(result.is_err());
            assert!(matches!(future.await, Err(McpCallError::Storage)));
            assert!(!owner.quiescent());
            let entry = owner.mcp.as_ref().unwrap().entry.as_ref().unwrap();
            assert!(matches!(entry.phase, Phase::Fault));
            assert!(entry.publication.is_some());
            assert!(owner.step(&mut writer, &target).is_err());
            assert_eq!(writer.snapshot().unwrap().raw.len(), 2);
        }
        assert_eq!(calls.load(Ordering::Acquire), 1);
        connection.shutdown().await.unwrap();
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn mcp_relay_retains_large_reply_within_the_complete_saved_envelope() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    let original = "x".repeat(32500);
    let (mut connection, calls, server) =
        fixture::ready(json!({"content":[{"type":"text","text":original}]}), false).await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = call(&connection);
    let mut future = Box::pin(port.call(
        &mut connection,
        operation,
        &mut results,
        std::future::pending(),
        |_| true,
    ));
    assert!(future.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    to_result_wait(&mut future, &owner).await;
    owner.step(&mut writer, &target).unwrap();
    let result = future.await.unwrap();
    assert_eq!(result.disposition, McpDisposition::Completed);
    assert!(result.permits_continuation());
    assert!(result.message.content.len() <= crate::mcp::results::MAX_VISIBLE_BYTES);
    let body: serde_json::Value = serde_json::from_str(&result.message.content).unwrap();
    let part = &body["reply"]["parts"][0];
    assert_eq!(part["kind"], "retained");
    assert_eq!(part["truncated"], true);
    assert!(!part["excerpt"].as_str().unwrap().is_empty());
    let handle = part["handle"].as_str().unwrap();
    let mut restored = String::new();
    let mut offset = 0;
    loop {
        let chunk = results
            .chunk(
                &port.binding,
                handle,
                offset,
                crate::mcp::results::MAX_VISIBLE_BYTES,
                Instant::now(),
            )
            .unwrap();
        restored.push_str(chunk.text.as_deref().unwrap());
        if !chunk.truncated {
            break;
        }
        assert!(chunk.next_offset > offset);
        offset = chunk.next_offset;
    }
    assert_eq!(restored, original);
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(owner.quiescent());
    assert_eq!(writer.snapshot().unwrap().raw.len(), 3);
    connection.shutdown().await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mcp_terminal_stop_cancels_process_slots_and_waits_for_their_cleanup() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    // Reuse the real process-owner intent path with expiry before spawn. No
    // helper runs; the existing Deferred hook holds its cleanup confirmation.
    owner.invalidate_after_intent = Some(false);
    let mut waiting = owner
        .port
        .submit_confined_read(&crate::desktop_execution::ConfinedReadRequest::DiffBefore {
            path: "dummy".into(),
        })
        .unwrap();
    owner.step(&mut writer, &target).unwrap();
    // Retain the completed handle: dropping it independently sets cancellation.
    (&mut waiting).await.unwrap();
    let cleanup = owner.inject_unconfirmed_cleanup();
    let process_cancelled = owner.slots[0].lock().unwrap().cancelled.clone();
    assert!(!process_cancelled.load(Ordering::Acquire));
    let (mut connection, calls, server) = fixture::ready(
        json!({"resultType":"input_required","inputRequests":{},"requestState":"once"}),
        false,
    )
    .await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = call(&connection);
    let mut future = Box::pin(port.call(
        &mut connection,
        operation,
        &mut results,
        std::future::pending(),
        |_| true,
    ));
    assert!(future.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    to_result_wait(&mut future, &owner).await;
    assert!(!process_cancelled.load(Ordering::Acquire));
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        future.await.unwrap().disposition,
        McpDisposition::InputRequired
    );
    assert!(process_cancelled.load(Ordering::Acquire));
    assert!(port.stop.is_set());
    assert!(!owner.quiescent());
    cleanup.store(true, Ordering::Release);
    owner.step(&mut writer, &target).unwrap();
    assert!(owner.quiescent());
    assert_eq!(
        owner.observation(Observation::Succeeded),
        Observation::Interrupted
    );
    writer
        .finish(
            &target,
            Observation::Interrupted,
            ResultId::new("coupled-terminal").unwrap(),
        )
        .unwrap();
    owner.saved = true;
    assert_eq!(calls.load(Ordering::Acquire), 1);
    connection.shutdown().await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn mcp_relay_unrepresentable_reply_after_send_is_unknown_not_before_send() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    let (mut connection, calls, server) = fixture::ready(
        json!({"content":vec![json!({"type":"image","data":"AA==","mimeType":"image/png"});17]}),
        false,
    )
    .await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = call(&connection);
    let mut future = Box::pin(port.call(
        &mut connection,
        operation,
        &mut results,
        std::future::pending(),
        |_| true,
    ));
    assert!(future.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    to_result_wait(&mut future, &owner).await;
    owner.step(&mut writer, &target).unwrap();
    let result = future.await.unwrap();
    assert_eq!(result.disposition, McpDisposition::OutcomeUnknown);
    assert!(!result.permits_continuation());
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(
        owner.observation(Observation::Cancelled),
        Observation::OutcomeUnknown
    );
    connection.shutdown().await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn host_prompt_integration_preserves_model_before_send_continuation() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    let (mut connection, calls, fixture) = fixture::ready(json!({"content":[]}), false).await;
    let mut results = ResultStore::new(port.binding.clone()).unwrap();
    let operation = call(&connection);
    let mut request = Box::pin(port.call(
        &mut connection,
        operation,
        &mut results,
        std::future::pending(),
        |_| false,
    ));
    assert!(request.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    decide(&mut writer, &mut owner, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    to_result_wait(&mut request, &owner).await;
    owner.step(&mut writer, &target).unwrap();
    let result = request.await.unwrap();
    assert_eq!(result.disposition, McpDisposition::BeforeSend);
    assert!(result.permits_continuation());
    assert!(!owner.closed && !port.stopped());
    assert!(owner.quiescent());
    assert_eq!(calls.load(Ordering::Acquire), 0);
    connection.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}
